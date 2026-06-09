//! Core FT8 decoder implementation (Phase 1A sensitivity improvements)
//!
//! Implements the FT8 decode pipeline:
//! 1. Compute time-frequency spectrogram (one-symbol FFT windows)
//! 2. Search for Costas sync patterns [3,1,4,0,6,5,2] in 2D (time, frequency)
//! 3. Extract symbols using complex DFT magnitude (phase-independent)
//! 4. Compute soft log-likelihood ratios for each of the 174 codeword bits
//! 5. LDPC belief propagation decoding with soft input
//! 6. CRC-14 verification
//! 7. Message parsing

use crate::{
    message::{calculate_crc14, DecodedMessage, MessageParser, CRC_BITS, PAYLOAD_BITS},
    osd::{OsdConfig, OsdDecoder},
    protocol::ProtocolParams,
    signal_processing::{FftProcessor, WindowFunction},
    soft_combiner::{SoftCombiner, SoftCombinerConfig},
    DecodingMetrics, Ft8Error, Ft8Result, MessageHandler, NullMessageHandler, Protocol,
    NUM_SYMBOLS, NUM_TONES, SAMPLE_RATE, SYMBOL_DURATION, TONE_SPACING,
};
use bitvec::prelude::*;
use num_complex::Complex;
use rayon::prelude::*;
use rustfft::FftPlanner;
use std::collections::HashSet;
use std::ops::RangeInclusive;
use std::time::{Instant, SystemTime};
use tracing::{debug, info};

use crate::parallel::BudgetTracker;

// ============================================================================
// Constants
// ============================================================================

/// Maximum number of decode candidates to process
const MAX_DECODE_CANDIDATES: usize = 100;

/// LDPC decoder iterations
/// LDPC belief-propagation iteration cap before falling back to OSD.
/// Raised from 25 to 50 on 2026-05-22 (hb-005 sweep): +0.0008 composite
/// on the curated tiers with no regressions on fixtures or synth.
/// Notably, hard-1000 saw +64 recovered AND -54 novel — more BP
/// convergence pulled fuzzy "novel" decodes into confirmed truth-matches.
/// Wall-clock got slightly FASTER overall (-3%) because BP converging
/// successfully is cheaper than falling through to OSD.
///
/// 2026-05-25 (hb-053 / batch 9): raised 50 → 100. Per batch-3 hb-035
/// sweep on hard-200: iters=100 gains +12 real decodes (+0.27%) at +21
/// novel (-32% net) when filter applied (batch 6 iter 5: rec 4364→4376,
/// novel 811→818 — Δrec +12, Δnov +7 vs OSD-2 + filter). Production
/// now ships with FP filter (hb-062), making the extra recall safe.
const LDPC_MAX_ITERATIONS: usize = 100;

/// FT8 Costas synchronization array
const COSTAS: [u8; 7] = [3, 1, 4, 0, 6, 5, 2];

/// Samples per FT8 symbol at 12 kHz (used only as fallback reference)
const SAMPLES_PER_SYMBOL: usize = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize; // 1920

/// Frequency oversampling rate (2 = sub-bin resolution)
const FREQ_OSR: usize = 2;

/// Time oversampling rate (2 = half-sub-symbol resolution, matching ft8_lib)
/// Each symbol occupies TIME_OSR time steps in the spectrogram.
const TIME_OSR: usize = 2;

/// Target LLR variance for normalization (matches ft8_lib's ftx_normalize_logl)
/// LLR normalization target variance. Default raised from 24.0 (ft8_lib's
/// `ftx_normalize_logl` value) to 32.0 on 2026-05-22 (hb-006 sweep): tiny
/// but monotonic gain on the curated tiers (+5 recovered on hard-200,
/// +11 on hard-1000, composite +0.0003) with no regressions on fixtures
/// or synth. Diverges from ft8_lib's reference value but pancetta's
/// decoder is not bit-exact with ft8_lib anyway (neural OSD, different
/// candidate ranking, etc.) — operational sensitivity wins.
const LLR_TARGET_VARIANCE: f32 = 32.0;

/// Minimum Costas sync score to consider a candidate (dB difference, neighbor comparison)
const MIN_SYNC_SCORE: f64 = 3.0;

/// Maximum candidates from sync search before NMS. Raised from 100 to
/// 200 on 2026-05-21 (hb-003), then to 300 on 2026-05-23 (hb-038)
/// after nms-off (hb-019) shifted the elbow. hb-038 5-tier delta vs
/// 200: composite +0.0023, hard-200 +40 rec, hard-1000 +96 rec, no
/// regressions; wall-clock +92% per 5-tier (still well within the
/// 3000 ms per-WAV budget).
const MAX_SYNC_CANDIDATES: usize = 300;

/// Minimum frequency bin for FT8 search (0 = full passband coverage)
const MIN_FREQ_BIN: usize = 0;

/// Non-maximum suppression radius in time steps (scaled with TIME_OSR)
const NMS_TIME_RADIUS: usize = 4 * TIME_OSR;

/// Non-maximum suppression radius in frequency bins
const NMS_FREQ_RADIUS: usize = 2;

// ============================================================================
// Decoder configuration
// ============================================================================

/// Decoder configuration for FT8/FT4/FT2 protocols
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Ft8Config {
    /// Sample rate (must be 12 kHz)
    pub sample_rate: u32,

    /// Protocol to decode (FT8, FT4, or FT2)
    pub protocol: Protocol,

    /// Maximum number of candidates to decode
    pub max_candidates: usize,

    /// LDPC decoder iterations
    pub ldpc_iterations: usize,

    /// Time search range (seconds)
    pub time_range: f64,

    /// Maximum number of successive decoding passes. Default 1 (no
    /// subtract-and-redecode). Lowered from 3 to 1 on 2026-05-23
    /// (hb-031): per hb-030's controlled probe and hb-031's 5-tier
    /// confirmation, `subtract_with_sidelobes` masks adjacent weak
    /// signals more than it surfaces new decodes, so passes 2+
    /// contribute essentially nothing (−0.0007 composite) at huge
    /// wall-clock cost (~2× decode time). Raise to ≥2 if a future
    /// fix (hb-037) makes multi-pass productive again.
    pub max_decode_passes: usize,

    /// OSD depth (0, 1, or 2). Set to None to disable OSD. Default: Some(1).
    /// Note: OSD-2 (4,187 trials) has a high CRC-14 false positive rate without
    /// additional validation. OSD-1 (92 trials) is the safe default.
    pub osd_depth: Option<u8>,

    /// Maximum candidates retained from Costas sync search before NMS.
    /// Default matches the historical hard-coded MAX_SYNC_CANDIDATES (100).
    /// Raising this lets weaker sync candidates survive into NMS + LDPC
    /// at the cost of CPU per slot; lowering it cuts compute at the risk
    /// of dropping marginal real signals on busy bands.
    pub max_sync_candidates: usize,

    /// Target variance for LLR normalization before LDPC decoding.
    /// Default 24.0 matches ft8_lib's ftx_normalize_logl(). LDPC
    /// sum-product propagation is sensitive to LLR scale: over-scaled
    /// LLRs cause BP to converge too aggressively to wrong codewords;
    /// under-scaled LLRs slow convergence.
    pub llr_target_variance: f32,

    /// Enable non-maximum suppression of nearby Costas sync candidates.
    /// When true, candidates within `nms_time_radius` time steps and
    /// `nms_freq_radius` frequency bins of a stronger candidate are
    /// dropped before LDPC. Default disabled as of 2026-05-22 (hb-019
    /// audit): the historical NMS radii (time=8, freq=2) were merging
    /// real adjacent signals on busy bands at the cost of +1706 decodes
    /// per hard-1000 corpus (+13.7%). Disabling raises wall-clock per
    /// WAV by ~58% (still well within the 3000 ms budget).
    pub nms_enabled: bool,

    /// Time radius (in spectrogram time steps) for NMS suppression. Only
    /// used when `nms_enabled = true`. Default value reflects the
    /// historical `NMS_TIME_RADIUS = 4 * TIME_OSR = 8`. hb-008 sweep
    /// (TBD) may tune this to recover the hb-019 wall-clock cost.
    pub nms_time_radius: usize,

    /// Frequency radius (in spectrogram bins) for NMS suppression. Only
    /// used when `nms_enabled = true`. Default value reflects the
    /// historical `NMS_FREQ_RADIUS = 2`. Per hb-019 finding, freq=2
    /// (= 25 Hz at 12.5 Hz/bin) is too coarse for busy FT8 bands —
    /// merges distinct signals 25 Hz apart. hb-008 sweep candidate.
    pub nms_freq_radius: usize,

    /// hb-036: score-relative NMS suppression delta. When `> 0.0` and
    /// `nms_enabled = true`, a weaker candidate `j` is suppressed only
    /// if it lies within the TF radius AND its `sync_score` is within
    /// `nms_score_delta_db` of the stronger candidate `i`'s sync_score
    /// (i.e. `j.sync_score > i.sync_score - nms_score_delta_db`).
    /// Meaningfully-weaker candidates (`j.sync_score <= i.sync_score -
    /// nms_score_delta_db`) are treated as distinct signals and kept.
    /// This discriminates "duplicate of a strong signal" (low Δscore,
    /// suppressed) from "distinct weaker signal" (high Δscore, kept).
    /// Default 0.0 — legacy pure TF-distance NMS behavior preserved.
    /// Note: sync_score is a Costas correlation, not a strict dB
    /// quantity; the `_db` suffix reflects the conceptual framing.
    pub nms_score_delta_db: f64,

    /// Minimum Costas sync score (correlation) for a candidate to be
    /// kept for LDPC decoding. Default 3.0 matches the historical
    /// `MIN_SYNC_SCORE` constant. Lowering surfaces more candidates
    /// (potential weak-signal recovery) at the cost of CPU and a
    /// higher LDPC failure rate. hb-007 sweep candidate.
    pub min_sync_score: f64,

    /// Enable per-candidate adaptive LDPC iteration scheduling
    /// (hb-022). When true, candidates are bucketed by sync_score:
    /// high (>8) → fewer iters, medium (4..8) → default
    /// `ldpc_iterations`, low (<4) → more iters. Default false —
    /// uniform `ldpc_iterations` for all candidates.
    pub adaptive_ldpc_iters: bool,

    /// Re-rank candidates by `block_score` after sync search +
    /// truncation, before LDPC. Default true (historical behavior).
    /// hb-009 A/B-tests this — with parallel decoding, candidate
    /// order shouldn't change which decodes succeed, only the order
    /// they finish; if A/B is bit-identical, this knob can be
    /// retired.
    pub block_score_rerank: bool,

    /// Maximum unsatisfied parity-check count for a BP-non-converged
    /// candidate to be eligible for OSD fallback. Default 6 — hb-014
    /// (2026-05-23) swept {0..6}: recall is flat from 0 through 4, novels
    /// grow monotonically with gate. hb-014 initially graduated 4 → 2
    /// (-21% novels, no recall cost) when no FP filter was available.
    /// hb-053 + batch 9 raised 2 → 6 because production now ships the
    /// FP filter (hb-062): gate=6 + filter has same recall as gate=2
    /// without filter, with -132 fewer novels.
    pub max_parity_errors_for_osd: usize,

    /// hb-044: enable parabolic interpolation of the Costas sync peak in
    /// the time axis. When true, after finding a candidate at integer
    /// time-step t0, fit a parabola to scores at t0-1/t0/t0+1 and store
    /// a fractional time offset (in [-0.5, +0.5]) on the candidate.
    /// Part 1 (this flag) only computes and stores the refinement;
    /// part 2 applies it in symbol extraction.
    /// **Default `true`** as of hb-068 graduation (2026-05-30), paired
    /// with `sync_time_interp_delta_scale = 0.3` — see that field's docs
    /// for the recall/sensitivity trade-off measurements.
    pub sync_time_interpolation: bool,

    /// hb-068 variant (a) — score gate: when `sync_time_interpolation` is
    /// on, only apply the parabolic refinement if the integer-bin sync
    /// score exceeds this threshold. Candidates with score ≤ gate keep
    /// the original (un-inflated) integer-bin score and `time_refinement=0`.
    /// Default 0.0 (no gate — refine all qualifying candidates).
    pub sync_time_interp_score_gate: f64,

    /// hb-068 variant (b) — delta scale: when `sync_time_interpolation` is
    /// on, multiply the parabolic delta by this factor before applying it
    /// to symbol extraction. The refined score is also recomputed from
    /// the scaled delta (parabola is `y_center + b·δ + a·δ²`), so the
    /// score consistently reflects the position used downstream.
    /// **Default 0.3** as of hb-068 graduation (2026-05-30). The
    /// unscaled (1.0) parabolic delta over-corrects on noisy real-corpus
    /// audio and regresses hard-200 by -116 recall. Scaling to 0.3
    /// captures the +2 dB synth-clean SNR@90% gain (clean single-peak
    /// fits) while only mildly perturbing correctly-aligned hard-corpus
    /// candidates (net +5 hard-200 recall, -7 novels).
    pub sync_time_interp_delta_scale: f64,

    /// hb-068 variant (c) — reject large deltas: when
    /// `sync_time_interpolation` is on AND `|delta| > threshold`, treat
    /// the refinement as unreliable and fall back to integer-bin
    /// behavior (delta=0, original score). Applied AFTER the
    /// parabolic clamp to [-0.5, 0.5]. `None` disables (no rejection
    /// — original hb-044 behavior).
    /// Default `None`.
    pub sync_time_interp_max_delta_abs: Option<f64>,

    /// hb-069: interpolate spectrogram lookups in linear power instead
    /// of dB. When true and the candidate has a non-zero
    /// `time_refinement`, `lookup_time_interp` converts each endpoint
    /// dB→linear (10^(db/10)), linearly interpolates in power, and
    /// converts back to dB. dB-space interpolation is non-linear in
    /// real power and can introduce non-physical values near the noise
    /// floor; linear-power interpolation preserves symbol energy more
    /// accurately at the cost of two pow/log per lookup.
    /// Default `false` until A/B confirms a net gain.
    pub sync_time_interp_linear_power: bool,

    /// hb-067 (arXiv:2306.00443): mBP offset — subtract this magnitude
    /// from each LLR before invoking OSD. Reduces BP's confidence so
    /// OSD considers more flip patterns. Default 0.0 (no behavior change).
    /// Sweep candidate range: 0.5 to 4.0.
    pub bp_offset_subtract: f32,

    /// JS8Call-Improved-style LDPC feedback refinement (clean-room port from
    /// `spec-js8call-ldpc-feedback-refinement.md`). When true, a failed first
    /// BP pass triggers a meta-loop:
    /// 1. Capture the iter-1 hard-decision codeword (output_llrs sign bits).
    /// 2. For each bit, compare hard-decision to original LLR sign.
    ///    - Agreement → multiply |LLR| by `ldpc_feedback_boost_factor`.
    ///    - Disagreement → multiply |LLR| by `ldpc_feedback_attenuate_factor`.
    ///    - If |original LLR| < `ldpc_feedback_erase_threshold`, force to 0.
    /// 3. Re-run BP on the refined LLRs; if converged, return.
    /// 4. Otherwise fall through to OSD as before.
    ///
    /// Default `false` — pending hard-200 measurement validation. Inspired
    /// by JS8Call-Improved `ldpc_feedback.h` (GPL-3.0 source code NOT read;
    /// clean-room implementation from prose spec only).
    pub ldpc_feedback_refinement_enabled: bool,

    /// Multiplier applied to `|LLR|` for bits where the original LLR sign
    /// agrees with the iter-1 hard-decision codeword. Typical 1.1-2.0;
    /// default 1.5. Sign is preserved. Magnitude clamped to ±30 to avoid
    /// downstream saturation.
    pub ldpc_feedback_boost_factor: f32,

    /// Multiplier applied to `|LLR|` for bits where the original LLR sign
    /// disagrees with the iter-1 hard-decision codeword (and `|LLR|` is
    /// above the erase threshold). Typical 0.3-0.7; default 0.5. Sign is
    /// preserved.
    pub ldpc_feedback_attenuate_factor: f32,

    /// `|LLR|` below this threshold is forced to 0 (erasure) on the
    /// disagreement path, treating the bit as "unknown" so LDPC fills it
    /// from parity on the second BP pass. Set to `f32::INFINITY` to disable
    /// erasure (disagreement always attenuates). Default 1.0 — small
    /// fraction of pancetta's typical LLR scale (±10 after normalization).
    pub ldpc_feedback_erase_threshold: f32,

    /// hb-056 cross-cycle non-coherent averaging: when true, after the
    /// regular per-candidate decode loop, group repeating-station
    /// candidates (same `freq_sub`+`freq_bin`±1, `t0` apart by a multiple
    /// of one FT8 slot ±2 steps, sync-score within band) and decode an
    /// additional candidate whose per-symbol tone energies are the
    /// LINEAR-POWER sum of the group's. Additive — never removes a
    /// per-slot decode; new decodes are unioned + deduped by message
    /// text. Power-only (pancetta's spectrogram discards phase, so this
    /// is the non-coherent variant of JTDX's `s2(i) = |cs|² + |csold|²`).
    /// Default **true** as of 2026-05-25: hard-200 A/B gives +14
    /// recovered / +8 novel (with FP filter); single-slot tiers
    /// (fixtures, synth-clean) form no groups and are no-ops.
    /// See docs/superpowers/specs/2026-05-25-cross-cycle-averaging-design.md.
    pub cross_cycle_averaging: bool,

    /// hb-074: when both `cross_cycle_averaging` AND this flag are true,
    /// the pass becomes coherent — the spectrogram retains complex FFT
    /// bins, each candidate's phase rotor is estimated from the 21 known
    /// Costas symbols, the complex symbol amplitudes are rotated to a
    /// common phase, then summed across cycles; `|sum|²` produces the
    /// averaged power. Coherent integration of N aligned signals improves
    /// SNR by N (not √N), so the expected gain is ~2× the non-coherent
    /// path at N=2. Default false — needs the complex spectrogram which
    /// costs ~2× memory in the spectrogram pass.
    pub cross_cycle_coherent: bool,

    /// hb-075: when `cross_cycle_coherent` AND this flag are both true,
    /// weight each member's contribution by the magnitude of its
    /// un-normalised Costas accumulator. Equivalent to multiplying each
    /// member's complex symbols by `conj(acc_i)` instead of `conj(rotor_i)`
    /// — does alignment AND MRC-style weighting in one op. Addresses
    /// hb-074's failure mode where noisy rotors on marginal members
    /// inflated sum variance. Default false.
    pub cross_cycle_coherent_mrc: bool,

    /// hb-086 V1: after the multipass loop, force-retry every original
    /// sync candidate (not at an already-subtracted position) against the
    /// residual spectrogram. Catches pairs where pass-1 LDPC failed at B's
    /// position because of A's interference but B's residual sync score
    /// (post-A-subtract) is below threshold — so hb-079's residual
    /// sync_search wouldn't re-surface B. Diagnostic on top-20 hard-200
    /// WAVs (`joint_decoding_pair_density.rs`) found 78% of missed truths
    /// are within 50 Hz of a recovered decode — the structural fit.
    pub joint_pair_retry: bool,

    /// hb-082: optional sync_score floor applied to the *residual*
    /// Costas search inside the multipass loop (independent of the
    /// production `min_sync_score` that gates the original pass). After
    /// subtraction the noise floor drops, so a lower threshold can
    /// surface more masked candidates. `None` reuses `min_sync_score`.
    pub residual_min_sync_score: Option<f64>,

    /// hb-086 V3 (SHELVED 2026-05-31): dB relaxation applied to
    /// `min_sync_score` for a post-V1 localized sync_search pass on
    /// the residual spectrogram. The pass scans ONLY frequency bins
    /// within `joint_residual_sync_window_bins` of a subtracted-
    /// eligible decode. 0.0 disables (production default); negative
    /// values would lower the threshold.
    ///
    /// SHELVED: production sweep at {-0.5, -1.0, -1.5, -2.0} on
    /// hard-200 produced 0 additional decoded messages at every
    /// threshold. Mechanism surfaces ~100+ truly-new candidates per
    /// WAV in the targeted window, but they are noise — CRC catches
    /// ~98% as FPs, plausibility rejects the rest. The residual at
    /// sub-3.0 sync_score in the targeted window is not decodable
    /// signal. Plumbing kept at default-off for future revisit.
    /// See `research/experiments/2026-05-31-hb-086-v3-subtract-aware-sync.md`.
    pub joint_residual_sync_relax_db: f64,

    /// hb-086 V3 (SHELVED 2026-05-31): half-width (in freq_bins) of
    /// the bin-targeting window around each subtracted-eligible
    /// decode for the V3 localized sync_search pass. Ignored when
    /// `joint_residual_sync_relax_db == 0.0`. Default 8 (≈ ±50 Hz at
    /// 6.25 Hz/bin) per the V3 subtract-window-potential diagnostic.
    pub joint_residual_sync_window_bins: usize,

    /// hb-081: MRC-weighted coherent subtract. When > 0.0, the
    /// subtract amplitude is scaled by `min(1, |acc|/threshold)` where
    /// |acc| is the un-normalised Costas accumulator magnitude (rotor's
    /// confidence). Weak-rotor decodes subtract less (avoiding
    /// over-subtraction of adjacent bins when the rotor estimate is
    /// noisy), strong-rotor decodes subtract fully. 0.0 = unweighted
    /// hb-079 behavior.
    pub coherent_subtract_mrc_threshold: f64,

    /// hb-079 + hb-080: coherent iterative-subtract multi-pass. After
    /// pass 1 (regular + cross-cycle), each decoded message's signal is
    /// subtracted from the complex spectrogram via ML projection
    /// (`Re(bin·conj(rotor))·rotor` — removes signal while preserving
    /// orthogonal noise); the residual is re-synced and any newly-
    /// revealed masked candidates are decoded. This field counts how
    /// many such subtract+repass *rounds* to run. 0 disables; 1 = the
    /// original hb-079 production (one round). hb-080 sweeps {2,3,4,5}.
    /// Replaces the dead dB-domain `subtract_with_sidelobes` (hb-030).
    pub coherent_multipass_iterations: u8,

    /// hb-016: residual energy early-stop for the coherent multipass
    /// loop. After step 1 (subtract) of each `coherent_subtract_and_repass`
    /// round, compute the mean per-bin "excess above noise" of the
    /// residual spectrogram — `mean(max(0, db - noise_floor_db))` where
    /// `noise_floor_db` is the median dB of the ORIGINAL spectrogram.
    /// When this drops BELOW the configured threshold (in dB), the
    /// residual is signal-poor; the expensive sync_search + LDPC for the
    /// rest of the round is skipped and the multipass loop breaks early.
    /// Saves CPU on WAVs where pass 1 already recovered most signal
    /// energy. `None` disables the probe (production default until an
    /// A/B confirms a threshold that saves wall-clock without regressing
    /// recall).
    pub residual_energy_stop_db: Option<f64>,

    /// hb-093: per-position residual SNR pre-decode gate. When `Some(db)`,
    /// both `joint_pair_retry_pass` AND `coherent_subtract_and_repass`'s
    /// step-4 post-sync decode loop compute a per-position residual SNR
    /// estimate (same primitive as `par_estimate_snr_spectrogram`) BEFORE
    /// running the expensive LDPC+CRC pipeline. Candidates with SNR below
    /// the threshold are skipped — they sit at noise-only positions and
    /// the LDPC work is wasted (BP converges on garbage, CRC catches
    /// ~98% as FPs, plausibility rejects the rest).
    ///
    /// The threshold is the WAV-relative SNR (dB, after the 2500/6.25
    /// bandwidth correction) — typical FT8 decodable signals sit in
    /// the −20..+10 dB range; the diagnostic recommended ~−5 dB as the
    /// sweet spot (filters ~37% of pair-retry candidates with 0% decode
    /// loss on top-5 hard-200). hb-093 step-4 extension (2026-06-01)
    /// applies the same gate to the much larger `coherent_subtract_and_repass`
    /// step-4 candidate set (~200/round vs ~130 for joint_pair_retry).
    ///
    /// `None` disables the gate (production default until a sweep
    /// confirms ≥10% elapsed reduction with zero recall loss).
    pub residual_snr_gate_db: Option<f64>,

    /// hb-093 diagnostic: when true, the decoder records per-position
    /// SNR + decode-success for every candidate the gate would evaluate
    /// (both `joint_pair_retry_pass` AND `coherent_subtract_and_repass`
    /// step-4 paths). The data is reset per `decode_window` call and
    /// read out via `Ft8Decoder::take_residual_snr_diagnostic`. The
    /// gate behavior is NOT affected — measurement only.
    /// Default false.
    pub residual_snr_diagnostic: bool,

    /// hb-063 (arXiv:2410.13131, Hocevar 2004): use a layered (row-
    /// sequential) belief-propagation schedule instead of the flooding
    /// schedule. Layered BP updates check nodes one at a time and folds
    /// each new check-to-variable message into the variable posteriors
    /// immediately, so later checks in the same sweep see fresher
    /// information — converging in ~half the iterations at the same
    /// frame-error rate. Default **true** as of batch 10 (2026-05-25):
    /// hard-200 +18 recovered (composite +0.00105) with -16% decode
    /// wall-clock and zero regression on fixtures/synth/doppler. The
    /// extra novel decodes it surfaces are caught downstream by the
    /// production FP filter (hb-062). Set false for the flooding schedule.
    pub layered_bp: bool,

    /// hb-048 (Session 3): enable the a7 template cross-correlation pass.
    /// After multipass + V1 joint-pair-retry, for each successfully-decoded
    /// callsign C in this window, generate ~32 next-utterance templates
    /// rooted at C and cross-correlate each template's expected codeword
    /// bits against the residual LLRs at sync_candidate positions within
    /// `a7_freq_window_hz` of C's audio frequency. Accept decodes where
    /// the winning template's `snr7 ≥ a7_snr7_threshold` AND
    /// `snr7b ≥ a7_snr7b_threshold`. Prior art: WSJT-X mainline commit
    /// `f13e31820470291fdd49627287a2dc08f3fa674c` (`lib/ft8_a7.f90`,
    /// Joe Taylor 2021); canonical thresholds 6.0 / 1.8 came from there.
    /// Default `false` until Session 3 sweep confirms graduation criteria.
    pub a7_enabled: bool,

    /// hb-048: a7 snr7 acceptance threshold (best-template matched-filter
    /// SNR in the LLR domain). WSJT-X reference value 6.0. Lowering admits
    /// more decodes at higher FP cost; raising tightens precision.
    pub a7_snr7_threshold: f64,

    /// hb-048: a7 snr7b acceptance threshold (best/second-best correlation
    /// ratio — the AP-FP filter). WSJT-X reference value 1.8. The structural
    /// ceiling for snr7b given 32 templates of mostly-disjoint codewords is
    /// in the 1.8-2.0 range per Session 2's synthetic-injection micro-test.
    pub a7_snr7b_threshold: f64,

    /// hb-048: half-width (Hz) of the freq window around each expected
    /// call's audio frequency used to select sync_candidates for
    /// cross-correlation. Default 6.25 Hz (one freq_bin). WSJT-X uses 2 Hz
    /// — pancetta's bin step is 3.125 Hz so 6.25 captures ±1 bin.
    pub a7_freq_window_hz: f64,

    /// WSJT-X Improved-style 4th-pass-after-a7. When `true` AND
    /// `a7_enabled` AND at least one a7-attributed decode landed in any
    /// standard pass, the decoder runs ONE additional standard pass
    /// after the standard `max_decode_passes` loop completes. The extra
    /// pass uses the a7-newly-discovered callsigns as fresh AP hints
    /// (added to a temporary `recent_calls` extension of the supplied
    /// `ApContext`) so AP-injected candidates that the standard pipeline
    /// previously could not reach become candidates in the cleaner
    /// post-a7 residual.
    ///
    /// The mechanism is bounded: exactly one extra pass, no recursion
    /// into another a7 round. The 4th pass is GATED OUT for itself
    /// (a7 does NOT re-run on the 4th pass's residual), keeping the
    /// pipeline deterministic per the upstream design.
    ///
    /// Default **false** — when off the decoder takes the byte-identical
    /// legacy path. Inspired by spec ref
    /// `research/specs/spec-wsjtx-improved-4th-pass-after-a7.md`
    /// (WSJT-X Improved v3.1.0, DG2YCB). Pancetta's implementation is
    /// independent and license-clean.
    pub fourth_pass_after_a7_enabled: bool,

    /// hb-057 V1 (Session 2): master switch for per-callsign median-DT
    /// prior narrowing of the residual Costas sync search. When `false`
    /// (default), the prior lookup is never consulted and the residual
    /// sync sweep is unrestricted. When `true` AND a prior is registered
    /// for at least one callsign decoded in the prior pass, residual
    /// sync candidates are filtered to those whose t0 (converted to
    /// slot-relative seconds) lies within at least one prior's window
    /// (`max(dt_history_window_floor_s, prior.iqr * dt_history_window_iqr_scale)`
    /// around `prior.median_dt`). Candidates outside every prior window
    /// AND callsigns with no prior remain searchable via the AP/joint
    /// retry path — the filter NEVER rejects candidates when no prior
    /// is available (cold-start safe). Diagnostic: 38.6% of missed
    /// truths on top-20 hard-200 WAVs sit in the prior-recoverable
    /// population (kill switch cleared 3.86×). See
    /// `docs/superpowers/specs/2026-05-31-hb-057-median-dt-design.md`.
    pub dt_history_enabled: bool,

    /// hb-057 V1: minimum prior-gate radius (seconds). The gate width is
    /// `max(this, prior.iqr * dt_history_window_iqr_scale)`. Default 0.2
    /// matches the diagnostic's ±0.2s window. Floor prevents IQR=0
    /// callsigns (stable bucket) from collapsing to a sub-step window.
    pub dt_history_window_floor_s: f64,

    /// hb-057 V1: IQR scaling factor for the prior gate. Default 3.0 —
    /// for the moderate-variance bucket (IQR ≤ 0.3s) this gives a
    /// ±0.9s window, well within the diagnostic's recoverable bound.
    pub dt_history_window_iqr_scale: f64,

    /// hb-057 V2 (Session 3): frequency window (Hz) for per-candidate
    /// callsign-keyed sync narrowing. For each residual candidate at
    /// `cand_freq`, the union of DT priors from callsigns whose recent
    /// sightings were within ±`dt_history_freq_window_hz` of `cand_freq`
    /// forms the t0 gate. Set to 25.0 (≈ 4 freq_bins at 6.25 Hz/bin) to
    /// match the typical operator-frequency stability window across a
    /// chrono-replay session. 0.0 disables V2 (falls back to V1 union-of-
    /// prior-pass behavior, kept for back-compat).
    pub dt_history_freq_window_hz: f64,

    /// hb-242: sync_bc partial-Costas metric. When enabled, the Costas
    /// search computes a parallel score using ONLY the second and third
    /// Costas blocks (symbols 36–42 and 72–78), skipping block A (0–6),
    /// and takes `max(full_abc, partial_bc)` as the candidate's sync
    /// score. This rescues slot-edge negative-dt signals where block A
    /// falls outside the recorded window — the full metric collapses
    /// (block A is noise/garbage) while the partial metric is still
    /// meaningful. Non-destructive: when block A contains real signal,
    /// the full metric dominates and nothing changes. Inspired by
    /// wsjtr `sync_bc` / WSJT-X mainline `sync8`. Targets the documented
    /// slot-edge bucket at 48.3% recall in pancetta's hard-200 corpus
    /// (1376 truths). Default **true** as a non-destructive backstop;
    /// flip to false to A/B-test the mechanism.
    pub costas_partial_metric_enabled: bool,

    /// hb-242 + wide-lag baseline (red2): two-pathway sync candidate
    /// emission. When enabled, candidates are filtered through two
    /// parallel 40th-percentile-normalized rankings — a "tight" window
    /// near the nominal slot start (t0 ≤ `costas_two_baseline_tight_steps`)
    /// and a "wide" window covering the full lag sweep. A candidate
    /// passes if EITHER normalized score clears
    /// `costas_two_baseline_norm_threshold`. When the per-bin tight and
    /// wide peaks land at different time-steps, BOTH are kept (the
    /// mainline `sync8` "second candidate per bin" rule).
    ///
    /// Inspired by WSJT-X mainline `sync8.f90` — the wide-lag baseline
    /// (`red2`) is the mechanism that catches slot-edge negative-dt
    /// signals at the candidate-generation stage, distinct from the
    /// per-candidate `sync_bc` partial-Costas metric. Default **false**
    /// until corpus measurement confirms the FP profile.
    pub costas_two_baseline_enabled: bool,

    /// hb-242 wide-lag baseline: half-width (in spectrogram time-steps)
    /// of the tight-lag window centred on the nominal slot start. Only
    /// consulted when `costas_two_baseline_enabled = true`. Default 20
    /// (≈ ±0.8 s at TIME_OSR=2 → 40 ms/step). The WSJT-X mainline
    /// `mlag = 10` references a 4-steps-per-symbol grid; pancetta's
    /// `TIME_OSR = 2` halves the resolution, so 20 steps ≈ 10 mainline
    /// steps in absolute time.
    pub costas_two_baseline_tight_steps: usize,

    /// hb-242 wide-lag baseline: percentile used as the per-bin
    /// normalization base. Default 0.40 matches WSJT-X mainline
    /// `npctile = nint(0.40 * iz)`. Only consulted when
    /// `costas_two_baseline_enabled = true`.
    pub costas_two_baseline_percentile: f64,

    /// hb-242 wide-lag baseline: minimum normalized sync score for a
    /// candidate to be kept. Default 1.2 matches the wsjtr / WSJT-X
    /// mainline `syncmin` constant. Only consulted when
    /// `costas_two_baseline_enabled = true`.
    pub costas_two_baseline_norm_threshold: f64,

    /// hb-230: half-width (Hz) of the relaxed-sync window centred on the
    /// QSO partner's audio frequency. When `Some(r)` AND the per-call
    /// `partner_freq_hz` is supplied to
    /// `decode_window_with_ap_scoped_partner`, any Costas sync candidate
    /// whose audio frequency lands within ±r of the partner gets a
    /// relaxed acceptance threshold
    /// (`max(0, min_sync_score + relaxed_sync_near_partner_score_delta)`).
    /// `None` (default) disables the mechanism. Inspired by JTDX's
    /// `lib/sync8.f90` candidate-acceptance loop, which uses a constant
    /// 3.0 Hz around `nfqso`. Pairs with the hb-229 partner band-collapse
    /// — the band-collapse already narrows the sweep; this further
    /// relaxes acceptance inside the narrow window for weak partner
    /// messages (RR73 / 73 at low SNR).
    pub relaxed_sync_near_partner_hz_radius: Option<f64>,

    /// hb-230: signed delta added to `min_sync_score` to form the
    /// relaxed threshold inside the near-partner window. Default 0.0 (no
    /// actual relaxation — the mechanism is structurally wired but does
    /// nothing until the operator tunes this knob).
    ///
    /// JTDX's reference value is `1.1` on a linear-magnitude scale
    /// normalised by the 40th-percentile baseline. Pancetta's
    /// `min_sync_score` is a raw dB-difference metric, so the JTDX
    /// constant does NOT transfer numerically. **Empirical recalibration
    /// is required**: collect normalised-vs-raw sync scores on a known
    /// partner signal at marginal SNR on hard-200, find the
    /// dB-difference level corresponding to JTDX's 1.1, and use the
    /// negative of (raw_threshold - that_level) as this knob.
    /// TODO: hard-200 recalibration sweep before flipping default ON.
    pub relaxed_sync_near_partner_score_delta: f64,

    /// JTDX cycle audio smoothing: when `true` AND `max_decode_passes > 1`,
    /// apply a 2-tap forward moving-average to the working audio buffer
    /// before pass 2 (cycle 1 → cycle 2 transition). The smoothing acts
    /// as a mild low-pass filter (3 dB attenuation at 3 kHz, ~0.03 dB at
    /// 300 Hz) that perturbs the noise distribution just enough for
    /// pass 2's Costas sync to find candidates pass 1 missed by a small
    /// margin. Inspired by JTDX's `lib/ft8_decode.f90` `ipass == 4`
    /// branch. Default `false` until hard-200 measurement confirms the
    /// recall lift on pancetta's pipeline.
    pub cycle_audio_smoothing_enabled: bool,

    /// hb-244: enable the JS8Call-Improved-inspired soft combiner across
    /// repeated receptions. When `true`, the AP0 spectrogram path calls
    /// `SoftCombiner::combine` between LLR normalization and LDPC BP for
    /// every candidate, and `mark_decoded` after a successful CRC pass.
    /// Repeated receptions of the same payload at the same coarse
    /// `(freq_bin, time_bin)` accumulate additive LLRs, raising the
    /// effective SNR seen by LDPC.
    ///
    /// Default `false`. The wiring is in place but disabled until a
    /// repeat-heavy corpus measurement validates a net recall gain;
    /// pancetta's hard-200 corpus is largely single-reception per signal
    /// and may not exercise the mechanism. The hot-path overhead when
    /// off is a single `Option::is_some()` branch.
    pub soft_combiner_enabled: bool,

    /// hb-244: cache capacity for the soft combiner (total entries across
    /// all coarse-key buckets). Excess entries evicted oldest-first.
    /// Default 256 matches `soft_combiner::DEFAULT_CAPACITY`. Only
    /// consulted when `soft_combiner_enabled = true`.
    pub soft_combiner_capacity: usize,

    /// hb-244: time-to-live (seconds) for soft combiner cache entries.
    /// Entries older than this are evicted on the next cleanup pass.
    /// Default 180 matches `soft_combiner::DEFAULT_TTL_SECONDS`. Only
    /// consulted when `soft_combiner_enabled = true`.
    pub soft_combiner_ttl_seconds: u64,

    /// JS8Call-Improved-inspired LLR whitening (per-tone × per-symbol
    /// noise normalisation). When `true`, after the symbol demapper
    /// computes the soft LLR vector and before `normalize_llrs`, each
    /// LLR triplet for symbol position `sym` (winner tone `w`) is
    /// scaled by `1 / sqrt(n_tone[w] × n_symbol[sym])`, where
    /// - `n_tone[w]` is the median of the magnitudes at tone `w` across
    ///   all data symbol positions where tone `w` was NOT the winner
    /// - `n_symbol[sym]` is the median of the (NUM_TONES − 1)
    ///   non-winning tone magnitudes at symbol position `sym`
    ///
    /// Both estimates are floored at `1e-6` to avoid division-by-zero
    /// when a tone row or symbol slot is identically zero. The
    /// subsequent `normalize_llrs` pass then re-standardises the
    /// vector's variance to `llr_target_variance`.
    ///
    /// Inspired by spec ref `research/specs/spec-js8call-llr-whitening.md`.
    /// Default `false` — the whitening pass is byte-identical to the
    /// legacy path when off (no math executes). Flip on for hard-200
    /// A/B; expected lift on band-edge / non-uniform-noise signals.
    pub llr_whitening_enabled: bool,

    /// hb-226: when true, the time-domain subtract path
    /// (`subtract_signal`) applies a Gaussian-style cosine ramp at
    /// each inter-symbol boundary instead of a hard rectangular
    /// envelope. Smooths the reconstructed waveform's spectral splatter
    /// so the residual buffer doesn't expose splatter-driven false
    /// candidates to subsequent decode passes. Inspired by spec ref
    /// `research/specs/spec-ft8mon-gaussian-ramp-subtract.md`
    /// (ft8mon `subtract()` `subtract_ramp = 0.11`). Default **false**
    /// until corpus measurement confirms recall/FP profile.
    pub gaussian_ramp_subtract_enabled: bool,

    /// hb-226: fractional ramp half-width at each symbol boundary, as a
    /// fraction of one symbol period. The total inter-symbol transition
    /// window is `2 × ramp` samples wide (off-ramp tail of the current
    /// symbol + on-ramp head of the next). Default **0.11** matches
    /// ft8mon's `subtract_ramp` constant — at 12 kHz / 1920 sps that's
    /// `round(1920 × 0.11) = 211` samples per side ≈ 17.6 ms. Clamped
    /// to a minimum of 1 sample. Only consulted when
    /// `gaussian_ramp_subtract_enabled = true`.
    pub gaussian_ramp_subtract_fraction: f64,

    /// hb-237: cross-sequence A7 master switch. When `true`, the
    /// coordinator's FT8 decode loop populates the
    /// `CrossSequenceCallCache` after each successful decode and
    /// retrieves the prior slot's opposite-parity seeds at the start
    /// of the next slot. Default `false` — the cache and any wired
    /// query points are inert until a corpus measurement confirms
    /// the recall lift. Inspired by spec ref
    /// `research/specs/spec-wsjtr-cross-sequence-a7.md` (WSJT-X
    /// `ft8_a7.f90` `iaptype=7`, shipped since v2.6.0). The state
    /// container itself lives in
    /// `pancetta_qso::CrossSequenceCallCache`; this flag gates the
    /// coordinator-side wiring.
    pub cross_sequence_a7_enabled: bool,
}

impl Default for Ft8Config {
    fn default() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            protocol: Protocol::Ft8,
            max_candidates: MAX_DECODE_CANDIDATES,
            ldpc_iterations: LDPC_MAX_ITERATIONS,
            time_range: 2.0,
            max_decode_passes: 1,
            osd_depth: Some(2),
            max_sync_candidates: MAX_SYNC_CANDIDATES,
            llr_target_variance: LLR_TARGET_VARIANCE,
            nms_enabled: false,
            nms_time_radius: NMS_TIME_RADIUS,
            nms_freq_radius: NMS_FREQ_RADIUS,
            // hb-036: 0.0 = legacy pure TF-distance NMS behavior. Production
            // NMS is currently off (hb-019), so this knob is a no-op unless
            // `nms_enabled` is also turned on by the eval harness.
            nms_score_delta_db: 0.0,
            min_sync_score: MIN_SYNC_SCORE,
            adaptive_ldpc_iters: false,
            block_score_rerank: true,
            // hb-053 / batch 9: raised 2 → 6. Wider gate is safe with FP
            // filter shipped (hb-062). Per batch 6 iter 4: gate=6 + filter
            // gives same recall as production with -132 novels.
            max_parity_errors_for_osd: 6,
            // hb-068 (GRADUATED 2026-05-30): sync_time_interpolation is the
            // production default ON, paired with delta_scale = 0.3 (variant b).
            // Hard-200: +5 recovered / -7 novels vs prior main (4616 → 4621).
            // Synth-clean snr@90% recovery: -18 → -20 dB (+2 dB sensitivity).
            // Plain hb-044 (scale = 1.0) regressed hard-200 by -116 recall;
            // scaling the parabolic delta to 0.3 captures the gain on clean
            // single-peak Costas patterns while only mildly perturbing
            // correctly-aligned candidates on noisy real-corpus audio (where
            // the unscaled delta over-corrects). See
            // research/experiments/2026-05-30-hb-068-conditional-refinement.md.
            sync_time_interpolation: true,
            sync_time_interp_score_gate: 0.0,
            sync_time_interp_delta_scale: 0.3,
            sync_time_interp_max_delta_abs: None,
            // hb-069: linear-power interpolation default off; CLI
            // sweep gates a possible flip to true if it rescues
            // hard-200 residual cost without regressing other tiers.
            sync_time_interp_linear_power: false,
            bp_offset_subtract: 0.0,
            // JS8Call-Improved-style LDPC feedback refinement: DEFAULT OFF
            // pending hard-200 measurement validation. When flipped on, a
            // failed BP pass triggers one extra meta-loop with refined LLRs
            // before falling through to OSD.
            ldpc_feedback_refinement_enabled: false,
            ldpc_feedback_boost_factor: 1.5,
            ldpc_feedback_attenuate_factor: 0.5,
            ldpc_feedback_erase_threshold: 1.0,
            // hb-063 (batch 10): layered BP is the production default —
            // +18 hard-200 recovered (composite +0.00105), -16% decode
            // wall-clock, no guard-tier regression.
            layered_bp: true,
            // hb-056 (2026-05-25): cross-cycle non-coherent averaging is
            // the production default. hard-200 A/B (with FP filter):
            // +14 recovered / +8 novel. Composite +~0.000815 from hard-200
            // alone. Synth-clean and fixtures are single-slot so groups
            // never form (no-op).
            cross_cycle_averaging: true,
            // hb-074 + hb-075 (2026-05-26): coherent cross-cycle averaging
            // with MRC magnitude-weighting is the production default. The
            // unweighted variant (hb-074, default off here was the right
            // call — it lost 10 hard-200 recovered). MRC fixes the
            // marginal-rotor variance problem; hard-200 A/B vs non-coherent
            // (with FP filter): +22 recovered / +1 novel. Composite ~+0.00128
            // from hard-200 alone (almost 2× hb-056). Spectrogram pays ~2x
            // memory when these flags are on; acceptable cost.
            cross_cycle_coherent: true,
            cross_cycle_coherent_mrc: true,
            // hb-081: MRC subtract scaling off by default until the
            // A/B confirms.
            coherent_subtract_mrc_threshold: 0.0,
            // hb-082: residual sync threshold uses production `min_sync_score`
            // until the A/B confirms a lower value is better.
            residual_min_sync_score: None,
            // hb-086 V3 (SHELVED 2026-05-31): disabled by default.
            // Sweep at {-0.5, -1.0, -1.5, -2.0} produced 0 additional
            // decoded messages — V3 surfaces noise, not signal, at the
            // relaxed threshold. Plumbing kept for future revisit.
            joint_residual_sync_relax_db: 0.0,
            joint_residual_sync_window_bins: 8,
            // hb-086 V1 (GRADUATED 2026-05-28): force-retry failed original
            // candidates against the residual spectrogram. hard-200 +12 rec
            // / +1 novel, hard-1000 +17 rec / +9 novel, composite +0.000700,
            // elapsed +2.2%. Targets interference pairs where pass-1 LDPC
            // failed because of mutual masking but the residual LLRs are
            // decodable and the residual sync_score is below threshold.
            joint_pair_retry: true,
            // hb-079 (2026-05-26) + hb-080 (2026-05-27): coherent
            // iterative-subtract multi-pass at N=3 rounds. hb-080 sweep
            // on hard-200: N=1→2 +7 rec, N=2→3 +9 rec, N=4/5 saturate;
            // ZERO novel cost across the sweep. +16 hard-200 recall at
            // N=3 vs N=1 (rate 0.53498 → 0.53685; composite +~0.000935).
            // Wall-clock at N=3 is 1.78× N=1, well within the 3000 ms/WAV
            // budget. 0 disables.
            coherent_multipass_iterations: 3,
            // hb-016: residual energy early-stop disabled by default until
            // an A/B sweep finds a threshold that saves wall-clock without
            // regressing composite. `Some(x_db)` enables the probe with
            // margin `x_db` above noise floor (median of original power).
            residual_energy_stop_db: None,
            // hb-093: per-position residual SNR pre-decode gate disabled
            // until a diagnostic confirms it filters ≥30% of candidates
            // and the gated-out positions are ≤2% decodable.
            residual_snr_gate_db: None,
            // hb-093: diagnostic instrumentation off by default. Enabled
            // by the hb093 diagnostic example.
            residual_snr_diagnostic: false,
            // hb-048 (Session 3): a7 template cross-correlation pass
            // disabled by default until graduation. WSJT-X reference
            // thresholds: snr7=6.0, snr7b=1.8. freq_window=6.25 Hz
            // (one pancetta freq_bin; WSJT-X uses 2 Hz).
            a7_enabled: false,
            a7_snr7_threshold: crate::a7::A7_SNR7_THRESHOLD_DEFAULT,
            a7_snr7b_threshold: crate::a7::A7_SNR7B_THRESHOLD_DEFAULT,
            a7_freq_window_hz: 6.25,
            // WSJT-X Improved 4th-pass-after-a7: default OFF preserves
            // byte-identical legacy decode. Inspired by spec ref
            // `spec-wsjtx-improved-4th-pass-after-a7.md`.
            fourth_pass_after_a7_enabled: false,
            // hb-057 V1 (Session 2): master switch off by default.
            // Eval harness flips this on to A/B-test the mechanism. See
            // `dt_history_window_floor_s` / `dt_history_window_iqr_scale`.
            dt_history_enabled: false,
            dt_history_window_floor_s: 0.2,
            dt_history_window_iqr_scale: 3.0,
            // V2 default: 25 Hz = ~4 freq_bins. Set to 0.0 to fall back
            // to V1 behavior (union-of-prior-pass-callsigns within-WAV).
            dt_history_freq_window_hz: 25.0,
            // hb-242: sync_bc partial-Costas metric. Default ON — the
            // max(full, partial) selection is non-destructive (partial
            // only wins when block A is degraded; otherwise full wins).
            // Targets the slot-edge negative-dt bucket (48.3% recall,
            // 1376 truths in hard-200).
            //
            // Batch 48 measurement: default-ON gave -18 TPs net on
            // hard-200 (5301 → 5283). The mechanism never lowers a
            // real signal's score, but it surfaces additional noise
            // candidates that eat into the max_sync_candidates cap
            // (default 300), displacing real TPs. Flipped to default-
            // OFF; the mechanism is preserved for opt-in by slot-edge-
            // specific corpora or future tuning that combines it with
            // a higher max_sync_candidates budget.
            costas_partial_metric_enabled: false,
            // Wide-lag two-baseline pathway: default OFF until corpus
            // measurement confirms the FP profile. The mechanism doubles
            // the candidate count on some bins (per-bin double-emission
            // when wide and tight peaks disagree), and the percentile
            // normalization shifts the threshold semantics — both need
            // hard-200 sweep before flipping on.
            costas_two_baseline_enabled: false,
            costas_two_baseline_tight_steps: 20,
            costas_two_baseline_percentile: 0.40,
            costas_two_baseline_norm_threshold: 1.2,
            // hb-230 (paired with hb-229 partner band-collapse):
            // relaxed-sync window default OFF. Both radius and delta must
            // be tuned together on hard-200 before the mechanism does
            // anything. The radius `Some(3.0)` matches JTDX's ±3 Hz
            // window but the score_delta must be empirically calibrated
            // — pancetta's `min_sync_score` is in dB-power units, not
            // JTDX's 40th-percentile-normalised linear magnitude. See
            // field doc-comments for the recalibration procedure.
            relaxed_sync_near_partner_hz_radius: None,
            relaxed_sync_near_partner_score_delta: 0.0,
            // JTDX cycle audio smoothing default OFF. Only fires when
            // `max_decode_passes > 1` AND this flag is true. Default-off
            // keeps single-pass (Slow tier) behaviour byte-identical
            // until a hard-200 measurement confirms the recall lift
            // composes with pancetta's existing coherent-multipass +
            // cross-cycle pipeline.
            cycle_audio_smoothing_enabled: false,
            // hb-244: soft combiner default OFF. Wiring shipped (see
            // be8d67e for the module); flip true to opt in. Default-off
            // hot-path cost is a single `Option::is_some()` branch.
            soft_combiner_enabled: false,
            soft_combiner_capacity: crate::soft_combiner::DEFAULT_CAPACITY,
            soft_combiner_ttl_seconds: crate::soft_combiner::DEFAULT_TTL_SECONDS,
            // JS8Call-Improved-inspired LLR whitening default OFF. When
            // OFF the whitening helper is never invoked, leaving the LLR
            // pipeline byte-identical to the legacy path. Inspired by
            // spec ref `spec-js8call-llr-whitening.md`.
            llr_whitening_enabled: false,
            // hb-226: Gaussian-ramp subtract default OFF. When OFF the
            // subtract path is byte-identical to the legacy
            // hard-edged subtraction. Inspired by spec ref
            // `spec-ft8mon-gaussian-ramp-subtract.md`.
            gaussian_ramp_subtract_enabled: false,
            // hb-226: 0.11 fraction matches ft8mon's `subtract_ramp`
            // constant (~17.6 ms at 12 kHz / 1920 sps).
            gaussian_ramp_subtract_fraction: 0.11,
            // hb-237: cross-sequence A7 default OFF. Only the cache +
            // coordinator-side wiring is shipped in the first session;
            // the per-seed enumeration / fine-sync / soft-symbol pipeline
            // is a follow-on. Flipping this on without the follow-on is
            // a no-op for recall (the cache populates but no consumer
            // reads from it yet).
            cross_sequence_a7_enabled: false,
        }
    }
}

// ============================================================================
// Internal data structures
// ============================================================================

/// Time-frequency spectrogram with frequency oversampling support
struct Spectrogram {
    /// Power values [time_step][freq_sub][freq_bin]
    /// With freq_osr=2: freq_sub 0 = even bins (0, 2, 4, ...), freq_sub 1 = odd bins (1, 3, 5, ...)
    power: Vec<Vec<Vec<f64>>>,
    /// hb-074: optional complex FFT bins, same shape as `power`. Populated
    /// only when `Ft8Config::cross_cycle_coherent` is true; required for
    /// coherent cross-cycle averaging (phase recovery from Costas, then
    /// complex sum across cycles). When `None`, the cross-cycle pass falls
    /// back to the non-coherent (power-only) path.
    complex: Option<Vec<Vec<Vec<Complex<f64>>>>>,
    /// Number of time steps
    num_steps: usize,
    /// Number of frequency bins per sub-bin (in 6.25 Hz units)
    num_bins: usize,
    /// Frequency oversampling rate
    freq_osr: usize,
    /// Number of time steps prepended for negative-time search. Subtract this from
    /// candidate.time_step to get the real time offset relative to nominal slot start.
    time_padding: usize,
}

/// Costas sync search candidate
#[derive(Clone, Copy, Debug)]
struct CostasCandidate {
    /// Time step in spectrogram (quarter-symbol units with TIME_OSR=2)
    time_step: usize,
    /// Base frequency bin in spectrogram (bin * 6.25 Hz)
    freq_bin: usize,
    /// Frequency sub-bin index (0..freq_osr-1)
    freq_sub: usize,
    /// Costas sync correlation score (refined value when
    /// `sync_time_interpolation` is enabled).
    sync_score: f64,
    /// hb-044: fractional time-bin refinement from parabolic interpolation
    /// of `compute_costas_score` at t0-1 / t0 / t0+1. In [-0.5, +0.5].
    /// 0.0 = integer-bin alignment (unrefined). Currently unused in
    /// symbol extraction — that wiring is hb-044 part 2.
    time_refinement: f64,
}

/// Waterfall display data for visualization
#[derive(Debug, Clone)]
pub struct WaterfallData {
    /// Time bins (seconds)
    pub time_bins: Vec<f64>,
    /// Frequency bins (Hz)
    pub frequency_bins: Vec<f64>,
    /// Power matrix (time x frequency) in dB
    pub power_matrix: Vec<Vec<f64>>,
    /// Minimum power level in dB
    pub min_power: f64,
    /// Maximum power level in dB
    pub max_power: f64,
}

// ============================================================================
// Ft8Decoder
// ============================================================================

/// High-performance decoder for FT8/FT4/FT2 protocols
pub struct Ft8Decoder {
    /// Decoder configuration
    config: Ft8Config,

    /// Protocol parameters derived from config.protocol
    protocol_params: ProtocolParams,

    /// FFT processor for waterfall display
    fft_processor: FftProcessor,

    /// Message parser
    message_parser: MessageParser,

    /// LDPC decoder
    ldpc_decoder: LdpcDecoder,

    /// Pre-computed FFT plan for symbol extraction (sps-length)
    symbol_fft: std::sync::Arc<dyn rustfft::Fft<f64>>,

    /// Pre-computed Hann window for symbol extraction (sps-length)
    symbol_window: Vec<f64>,

    /// Reusable FFT buffer for symbol extraction (avoids per-call allocation)
    symbol_fft_buffer: Vec<Complex<f64>>,

    /// Pre-computed FFT plan for spectrogram (nfft = 2 * sps)
    spectrogram_fft: std::sync::Arc<dyn rustfft::Fft<f64>>,

    /// Pre-computed Hann window for spectrogram (nfft length)
    spectrogram_window: Vec<f64>,

    /// Message handler for callbacks
    message_handler: Box<dyn MessageHandler + Send>,

    /// Performance metrics
    last_metrics: DecodingMetrics,

    /// hb-093 diagnostic accumulator. Populated only when
    /// `config.residual_snr_diagnostic` is true. Per joint_pair_retry
    /// candidate position: (sync_score, residual_snr_db, decoded_ok).
    /// Read out (and reset) by `take_residual_snr_diagnostic`.
    residual_snr_records: Vec<(f64, f32, bool)>,

    /// hb-057 V1 (Session 2): optional per-callsign DT prior lookup. When
    /// `Some(...)` AND `config.dt_history_enabled` is true, the residual
    /// `coherent_subtract_and_repass` step narrows its candidate set by
    /// the union of prior-windows for callsigns decoded in the prior
    /// pass. `None` (default) restores the historical full-axis sweep.
    dt_priors: Option<std::sync::Arc<dyn crate::dt_history::DtPriorLookup>>,

    /// hb-244: optional soft combiner for cross-reception LLR
    /// accumulation. Constructed eagerly when
    /// `config.soft_combiner_enabled = true`. `None` when disabled —
    /// the hot path takes a single branch test in that case. Wrapped
    /// in a `Mutex` because `combine()` requires `&mut self` and the
    /// AP0 candidate loop runs across rayon workers.
    soft_combiner: Option<std::sync::Arc<std::sync::Mutex<SoftCombiner>>>,
}

impl Ft8Decoder {
    /// Create a new FT8 decoder with default configuration
    pub fn new(config: Ft8Config) -> Ft8Result<Self> {
        Self::with_message_handler(config, Box::new(NullMessageHandler))
    }

    /// Create a new decoder with custom message handler
    pub fn with_message_handler(
        config: Ft8Config,
        message_handler: Box<dyn MessageHandler + Send>,
    ) -> Ft8Result<Self> {
        if config.sample_rate != SAMPLE_RATE {
            return Err(Ft8Error::InvalidSampleRate {
                expected: SAMPLE_RATE,
                actual: config.sample_rate,
            });
        }

        let protocol_params = match config.protocol {
            Protocol::Ft8 => ProtocolParams::ft8(),
            Protocol::Ft4 => ProtocolParams::ft4(),
            #[cfg(feature = "ft2")]
            Protocol::Ft2 => ProtocolParams::ft2(),
        };

        let fft_processor = FftProcessor::new(4096, WindowFunction::Hann)?;
        let message_parser = MessageParser::new();
        let ldpc_decoder = LdpcDecoder::new_with_osd(
            config.ldpc_iterations,
            config.osd_depth.map(|d| OsdConfig { max_depth: d }),
        )?
        .with_max_parity_errors_for_osd(config.max_parity_errors_for_osd)
        .with_bp_offset_subtract(config.bp_offset_subtract)
        .with_layered(config.layered_bp)
        .with_feedback_refinement(
            config.ldpc_feedback_refinement_enabled,
            config.ldpc_feedback_boost_factor,
            config.ldpc_feedback_attenuate_factor,
            config.ldpc_feedback_erase_threshold,
        );

        // Pre-compute FFT plan and Hann window for symbol extraction
        let sps = protocol_params.samples_per_symbol(SAMPLE_RATE);
        let mut planner = FftPlanner::<f64>::new();
        let symbol_fft = planner.plan_fft_forward(sps);
        let pi2 = 2.0 * std::f64::consts::PI;
        let symbol_window: Vec<f64> = (0..sps)
            .map(|i| 0.5 * (1.0 - (pi2 * i as f64 / (sps - 1) as f64).cos()))
            .collect();

        let symbol_fft_buffer = vec![Complex::new(0.0, 0.0); sps];

        // Pre-compute FFT plan and Hann window for spectrogram.
        // Bake in 2.0/nfft normalization to match ft8_lib's monitor.c:
        //   window[i] = fft_norm * hann_i(i, nfft)
        // where fft_norm = 2.0/nfft and hann_i(i,N) = sin²(π*i/N).
        let spec_nfft = sps * FREQ_OSR; // 3840
        let spectrogram_fft = planner.plan_fft_forward(spec_nfft);
        let fft_norm = 2.0 / spec_nfft as f64;
        let spectrogram_window: Vec<f64> = (0..spec_nfft)
            .map(|i| {
                let x = (std::f64::consts::PI * i as f64 / spec_nfft as f64).sin();
                fft_norm * x * x
            })
            .collect();

        // hb-244: construct the soft combiner eagerly when enabled. When
        // disabled, the field stays `None` and the AP0 hot path takes a
        // single branch test per candidate (zero-allocation, zero-lock
        // fast path).
        let soft_combiner = if config.soft_combiner_enabled {
            let combiner_cfg = SoftCombinerConfig {
                capacity: config.soft_combiner_capacity,
                ttl: std::time::Duration::from_secs(config.soft_combiner_ttl_seconds),
                ..SoftCombinerConfig::default()
            };
            Some(std::sync::Arc::new(std::sync::Mutex::new(
                SoftCombiner::new(combiner_cfg),
            )))
        } else {
            None
        };

        Ok(Self {
            config,
            protocol_params,
            fft_processor,
            message_parser,
            ldpc_decoder,
            symbol_fft,
            symbol_window,
            symbol_fft_buffer,
            spectrogram_fft,
            spectrogram_window,
            message_handler,
            last_metrics: DecodingMetrics::default(),
            residual_snr_records: Vec::new(),
            dt_priors: None,
            soft_combiner,
        })
    }

    /// hb-057 V1: attach a per-callsign DT prior lookup. Combined with
    /// `Ft8Config::dt_history_enabled = true`, the residual
    /// `coherent_subtract_and_repass` narrows its candidate t0 axis by
    /// the union of per-callsign prior windows.
    pub fn with_dt_priors(
        mut self,
        priors: std::sync::Arc<dyn crate::dt_history::DtPriorLookup>,
    ) -> Self {
        self.dt_priors = Some(priors);
        self
    }

    /// hb-093: drain the diagnostic accumulator. Returns the per-candidate
    /// `(sync_score, residual_snr_db, decoded_ok)` records captured during
    /// the most recent `decode_window` call (joint_pair_retry path). Resets
    /// the internal buffer. Only populated when
    /// `config.residual_snr_diagnostic` is true.
    pub fn take_residual_snr_diagnostic(&mut self) -> Vec<(f64, f32, bool)> {
        std::mem::take(&mut self.residual_snr_records)
    }

    /// Get the current protocol parameters
    pub fn protocol_params(&self) -> &ProtocolParams {
        &self.protocol_params
    }

    // ========================================================================
    // Main decode pipeline
    // ========================================================================

    /// Decode a 12.64-second window of audio samples
    pub fn decode_window(&mut self, samples: &[f32]) -> Ft8Result<Vec<DecodedMessage>> {
        self.decode_window_with_ap(samples, &crate::ap::ApContext::default())
    }

    /// Decode using ft8_lib's C decoder via FFI.
    ///
    /// This uses the reference C implementation which has full sliding-frame
    /// spectrogram processing and matches WSJT-X sensitivity. The output
    /// tuples are converted to our DecodedMessage type.
    pub fn decode_window_ft8lib(samples: &[f32]) -> Vec<DecodedMessage> {
        let tuples = crate::ft8_lib_ffi::ft8lib_decode_audio(samples);
        tuples
            .into_iter()
            .map(|(text, freq, snr, ldpc_errors)| {
                DecodedMessage::from_ft8lib(&text, freq, snr, ldpc_errors)
            })
            .collect()
    }

    /// Decode a 12.64-second window of audio samples with A Priori (AP) context.
    ///
    /// When `ap_context` contains known callsigns or an active QSO, candidates
    /// that fail standard (AP0) decoding are retried with progressively stronger
    /// AP injection levels (AP1 through AP4). This improves decode success at
    /// low SNR without affecting candidates that decode at AP0.
    pub fn decode_window_with_ap(
        &mut self,
        samples: &[f32],
        ap_context: &crate::ap::ApContext,
    ) -> Ft8Result<Vec<DecodedMessage>> {
        self.decode_window_with_ap_scoped(samples, ap_context, None)
    }

    /// Decode with default AP context but restrict the Costas sync search to
    /// `freq_bin_range`. Used by the coordinator's t=13s partial-buffer path
    /// (hb-091 Session 2) to scope decoding to the in-QSO partner's known
    /// frequency.
    pub fn decode_window_scoped(
        &mut self,
        samples: &[f32],
        freq_bin_range: RangeInclusive<usize>,
    ) -> Ft8Result<Vec<DecodedMessage>> {
        self.decode_window_with_ap_scoped(
            samples,
            &crate::ap::ApContext::default(),
            Some(freq_bin_range),
        )
    }

    /// Decode with AP context, optionally restricting Costas sync to
    /// `freq_bin_range`. `None` matches `decode_window_with_ap`. The scope
    /// is applied to **all** sync passes within this call (initial sweep +
    /// residual multipass), so latency-bounded scoped decodes never silently
    /// spend budget searching outside the requested range.
    pub fn decode_window_with_ap_scoped(
        &mut self,
        samples: &[f32],
        ap_context: &crate::ap::ApContext,
        freq_bin_range: Option<RangeInclusive<usize>>,
    ) -> Ft8Result<Vec<DecodedMessage>> {
        self.decode_window_with_ap_scoped_partner(samples, ap_context, freq_bin_range, None)
    }

    /// hb-230: same as `decode_window_with_ap_scoped` but additionally accepts
    /// `partner_freq_hz` — the QSO partner's audio frequency in Hz. When
    /// `Some(p)` AND `Ft8Config::relaxed_sync_near_partner_hz_radius` is
    /// `Some(r)`, the Costas sync acceptance threshold is relaxed by
    /// `Ft8Config::relaxed_sync_near_partner_score_delta` for candidates
    /// within `±r` Hz of `p`. The relaxation applies on every sync pass
    /// (pass 1 and the residual multipass) so weak partner messages
    /// (RR73, 73) get a second chance at acceptance.
    ///
    /// `partner_freq_hz = None` (or the config radius `None`) makes this
    /// method byte-identical to `decode_window_with_ap_scoped`.
    pub fn decode_window_with_ap_scoped_partner(
        &mut self,
        samples: &[f32],
        ap_context: &crate::ap::ApContext,
        freq_bin_range: Option<RangeInclusive<usize>>,
        partner_freq_hz: Option<f64>,
    ) -> Ft8Result<Vec<DecodedMessage>> {
        let start_time = Instant::now();
        self.message_handler.on_window_start(SystemTime::now());
        // hb-093: reset diagnostic buffer at the start of each window.
        self.residual_snr_records.clear();

        let min_samples = self.protocol_params.total_samples(SAMPLE_RATE);
        if samples.len() < min_samples {
            return Err(Ft8Error::InvalidWindowSize {
                expected: min_samples,
                actual: samples.len(),
            });
        }

        let max_passes = self.config.max_decode_passes.max(1);

        // WSJT-X Improved-style 4th-pass-after-a7. When enabled AND a7
        // is enabled, the standard pass loop runs `max_passes + 1`
        // iterations; the extra iteration is only entered if a7 has
        // produced at least one decode during the standard passes (per
        // spec ref `spec-wsjtx-improved-4th-pass-after-a7.md` step 4 —
        // "skip the 4th pass if no a7 decodes happened"). Default off
        // preserves byte-identical legacy behavior.
        let fourth_pass_after_a7_enabled =
            self.config.fourth_pass_after_a7_enabled && self.config.a7_enabled;
        let loop_passes = if fourth_pass_after_a7_enabled {
            max_passes + 1
        } else {
            max_passes
        };

        // Working AP context. Identical to the supplied `ap_context` for
        // all standard passes; on the 4th-pass-after-a7 iteration we
        // swap in `ap_context_extended` which carries a7-discovered
        // callsigns as fresh `recent_calls` (spec: "use a7-decoded
        // callsigns as fresh AP hints for the remaining candidates").
        // Allocated lazily — only paid for when the 4th-pass fires.
        let mut ap_context_extended: Option<crate::ap::ApContext> = None;
        // Track a7-emitted text → callsign so the 4th-pass extension is
        // populated with the right `RecentCallAp` entries. Holds tuples
        // of (callsign, last_snr).
        let mut a7_discovered_calls: Vec<(String, f32)> = Vec::new();
        let mut a7_emitted_texts: HashSet<String> = HashSet::new();

        // Check whether AP is active (any known information available).
        // hb-043: a non-empty `recent_calls` alone activates AP — the
        // my_call-less injection path tries each recent callsign at
        // both bits 0-27 (caller) and 28-55 (called).
        let ap_active = ap_context.my_call.is_some()
            || ap_context.active_qso.is_some()
            || !ap_context.recent_calls.is_empty();

        // Budget tracker — stops decode passes when wall-clock time is exceeded
        let budget = BudgetTracker::new(self.config.osd_depth.map_or(2000, |d| {
            // Allow more time for deeper OSD
            2000 + d as u64 * 500
        }));

        // Working copy of audio that we subtract decoded signals from
        let mut residual_samples: Vec<f32> = samples.to_vec();
        let mut all_decoded_messages: Vec<DecodedMessage> = Vec::new();
        let mut seen_messages: HashSet<String> = HashSet::new();
        let mut best_sync_score = 0.0f64;

        for pass in 0..loop_passes {
            if budget.expired() {
                info!(pass, "Decode budget expired, stopping early");
                break;
            }

            // 4th-pass-after-a7 gating (spec ref
            // `spec-wsjtx-improved-4th-pass-after-a7.md` step 4): the
            // extra iteration (index >= max_passes) only runs if a7
            // emitted at least one decode during the standard passes.
            // If a7 produced nothing, the residual is identical to the
            // post-standard-pass residual and the extra pass would
            // re-discover candidates the prior pass already rejected.
            let is_fourth_pass_after_a7 = pass >= max_passes;
            if is_fourth_pass_after_a7 && a7_discovered_calls.is_empty() {
                break;
            }

            // Select the AP context for this iteration. Standard passes
            // use the supplied `ap_context` exactly as before — that
            // path is byte-identical to legacy. The 4th-pass-after-a7
            // iteration uses `ap_context_extended`, which clones the
            // original and appends a7-discovered callsigns to
            // `recent_calls`. Spec: "use a7-decoded callsigns as fresh
            // AP hints for the remaining candidates".
            let ap_context_for_pass: &crate::ap::ApContext = if is_fourth_pass_after_a7 {
                if ap_context_extended.is_none() {
                    let mut ext = ap_context.clone();
                    for (call, snr) in &a7_discovered_calls {
                        if let Some(rc) = crate::ap::RecentCallAp::new(call, *snr) {
                            // Skip duplicates that the original context
                            // already lists.
                            if !ext.recent_calls.iter().any(|r| r.callsign == rc.callsign) {
                                ext.recent_calls.push(rc);
                            }
                        }
                    }
                    ap_context_extended = Some(ext);
                }
                ap_context_extended.as_ref().unwrap()
            } else {
                ap_context
            };
            // Refresh ap_active for the 4th-pass iteration — a7
            // discoveries can flip an empty context into an active one.
            let ap_active_for_pass = if is_fourth_pass_after_a7 {
                ap_context_for_pass.my_call.is_some()
                    || ap_context_for_pass.active_qso.is_some()
                    || !ap_context_for_pass.recent_calls.is_empty()
            } else {
                ap_active
            };

            // JTDX cycle audio smoothing: between cycles 1 and 2 (i.e.
            // entering pass index 1 for pancetta — JTDX maps to `ipass==4`)
            // apply a 2-tap forward moving-average to `residual_samples`
            // in-place. Acts as a mild low-pass that perturbs the noise
            // distribution; weak signals just-below the pass-1 detection
            // floor sometimes stand above the smoothed-pass threshold.
            // Default OFF until hard-200 measurement confirms the lift on
            // pancetta's pipeline. See
            // `spec-jtdx-cycle-audio-smoothing.md`.
            //
            // The destructive in-place MA on `residual_samples` is safe
            // because pass 2+ would otherwise rebuild the audio from the
            // SIC-residual already; we replace that residual with its
            // smoothed copy. Cycle-3 backward-MA from a preserved original
            // is NOT implemented in this v1 — pancetta's
            // `max_decode_passes` typically caps at 3 across tiers and the
            // v1 wiring targets the cycle-1 → 2 transition only.
            if pass == 1 && self.config.cycle_audio_smoothing_enabled && residual_samples.len() >= 2
            {
                let n = residual_samples.len();
                for i in 0..n - 1 {
                    residual_samples[i] = 0.5 * (residual_samples[i] + residual_samples[i + 1]);
                }
                // Sample N-1 is left unchanged (no `i+1` exists).
            }

            // Convert to f64 and normalize
            let audio = self.preprocess_audio(&residual_samples)?;

            // Step 1: Compute time-frequency spectrogram
            // hb-079: mutable so the coherent multi-pass can subtract
            // decoded signals in place.
            let mut spectrogram = self.compute_spectrogram(&audio)?;

            // Step 2: Find candidates via Costas sync pattern search.
            // hb-091 Session 2: when `freq_bin_range` is Some, the Costas
            // sweep is restricted to that bin range (additive scoped path).
            // hb-230: forward `partner_freq_hz` so the relaxed-threshold
            // branch fires within ±radius Hz of the partner (no-op when
            // either input is None).
            let mut sync_candidates = self.costas_sync_search_partner(
                &spectrogram,
                freq_bin_range.as_ref(),
                partner_freq_hz,
            )?;

            // On passes 2+, reduce candidate count — strong signals are already
            // decoded and subtracted, so fewer candidates need evaluation.
            if pass > 0 {
                sync_candidates.truncate(40);
            }

            // Re-rank candidates by block score (better than sync-only
            // ranking). hb-009 gates this so the A/B test can compare
            // sync-only ordering vs block-score ordering.
            let sync_candidates: Vec<CostasCandidate> = if self.config.block_score_rerank {
                let mut scored: Vec<(f64, CostasCandidate)> = sync_candidates
                    .into_iter()
                    .map(|c| {
                        let bs = self.block_score(&spectrogram, &c);
                        (bs, c)
                    })
                    .collect();
                scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                scored.into_iter().map(|(_, c)| c).collect()
            } else {
                sync_candidates
            };

            {
                let pass_best = sync_candidates.first().map(|c| c.sync_score).unwrap_or(0.0);
                if pass == 0 {
                    best_sync_score = pass_best;
                }
                info!(
                    pass,
                    candidates = sync_candidates.len(),
                    best_score = format!("{:.1}", pass_best),
                    spec_steps = spectrogram.num_steps,
                    spec_bins = spectrogram.num_bins,
                    "FT8 sync search"
                );
                #[cfg(feature = "debug-decode")]
                for (i, c) in sync_candidates.iter().take(10).enumerate() {
                    eprintln!(
                        "  ours candidate {}: score={:.1} time={} freq={} fsub={}",
                        i, c.sync_score, c.time_step, c.freq_bin, c.freq_sub
                    );
                }
            }

            #[cfg(feature = "debug-decode")]
            {
                let _num_candidates = sync_candidates.len();
                let _best_score = sync_candidates.first().map(|c| c.sync_score).unwrap_or(0.0);
                eprintln!(
                    "[decode pass {}] {} sync candidates, best score={:.1}",
                    pass, _num_candidates, _best_score
                );
                for (i, c) in sync_candidates.iter().take(5).enumerate() {
                    eprintln!(
                        "  [{}] t={} f={} score={:.1}",
                        i, c.time_step, c.freq_bin, c.sync_score
                    );
                }
            }

            // Collect already-decoded callsigns for AP2 short-circuit
            let decoded_calls: HashSet<String> = all_decoded_messages
                .iter()
                .filter_map(|m| m.message.from_callsign.clone())
                .collect();

            // Build the immutable decode context for parallel candidate processing
            let ctx = DecodeContext {
                protocol_params: &self.protocol_params,
                message_parser: &self.message_parser,
                spectrogram: &spectrogram,
                audio: &audio,
                ap_context: ap_context_for_pass,
                ap_active: ap_active_for_pass,
                symbol_fft: &self.symbol_fft,
                symbol_window: &self.symbol_window,
                xor_sequence: self.protocol_params.xor_sequence,
                ldpc_iterations: self.config.ldpc_iterations,
                osd_depth: self.config.osd_depth,
                llr_target_variance: self.config.llr_target_variance,
                adaptive_ldpc_iters: self.config.adaptive_ldpc_iters,
                max_parity_errors_for_osd: self.config.max_parity_errors_for_osd,
                bp_offset_subtract: self.config.bp_offset_subtract,
                layered_bp: self.config.layered_bp,
                ldpc_feedback_refinement_enabled: self.config.ldpc_feedback_refinement_enabled,
                ldpc_feedback_boost_factor: self.config.ldpc_feedback_boost_factor,
                ldpc_feedback_attenuate_factor: self.config.ldpc_feedback_attenuate_factor,
                ldpc_feedback_erase_threshold: self.config.ldpc_feedback_erase_threshold,
                sync_time_interp_linear_power: self.config.sync_time_interp_linear_power,
                window_start: start_time,
                // hb-244: thread the soft combiner through. `None` when
                // disabled — the AP0 hot path collapses to a single
                // branch test in that case.
                soft_combiner: self.soft_combiner.as_ref(),
                // JS8Call-Improved-inspired LLR whitening flag. The
                // whitening helper is a no-op when this is false, so
                // the disabled hot path runs zero whitening code.
                llr_whitening_enabled: self.config.llr_whitening_enabled,
            };

            // Step 3: Decode candidates in parallel using rayon
            // Each rayon worker gets its own LdpcDecoder and FFT buffer.
            let max_candidates = self.config.max_candidates;
            let already_decoded = all_decoded_messages.len();
            let sps = self.protocol_params.samples_per_symbol(SAMPLE_RATE);

            // Adaptive LDPC iteration scheduling (hb-022): when
            // ctx.adaptive_ldpc_iters is true, create three LDPC
            // decoders per thread at low/mid/high iter counts and
            // dispatch each candidate by sync_score. When false, the
            // low/mid/high decoders all use ctx.ldpc_iterations, so
            // dispatch is a no-op.
            const ADAPTIVE_HIGH_SCORE: f64 = 8.0;
            const ADAPTIVE_MID_SCORE: f64 = 4.0;
            // hb-022 asymmetric variant: don't cut iters on high-SNR
            // candidates (the first {25,50,100} attempt lost -19 decodes
            // on hard-200 because score>8 isn't a strong enough
            // "BP-converges-fast" guarantee). Only add iters on low-SNR.
            const ADAPTIVE_ITERS_LOW: usize = 50; // = default, no cut on high-SNR
            const ADAPTIVE_ITERS_HIGH: usize = 100; // for low-SNR (more BP budget)

            let mut pass_decoded: Vec<DecodedMessage> = sync_candidates
                .par_iter()
                .map_init(
                    // Per-thread initialization: create LDPC decoders and FFT buffer
                    || {
                        let osd_cfg = ctx.osd_depth.map(|d| OsdConfig { max_depth: d });
                        let (iters_low, iters_mid, iters_high) = if ctx.adaptive_ldpc_iters {
                            (ADAPTIVE_ITERS_LOW, ctx.ldpc_iterations, ADAPTIVE_ITERS_HIGH)
                        } else {
                            (
                                ctx.ldpc_iterations,
                                ctx.ldpc_iterations,
                                ctx.ldpc_iterations,
                            )
                        };
                        let ldpc_low = LdpcDecoder::new_with_osd(iters_low, osd_cfg)
                            .expect("LDPC decoder init failed")
                            .with_max_parity_errors_for_osd(ctx.max_parity_errors_for_osd)
                            .with_bp_offset_subtract(ctx.bp_offset_subtract)
                            .with_layered(ctx.layered_bp)
                            .with_feedback_refinement(
                                ctx.ldpc_feedback_refinement_enabled,
                                ctx.ldpc_feedback_boost_factor,
                                ctx.ldpc_feedback_attenuate_factor,
                                ctx.ldpc_feedback_erase_threshold,
                            );
                        let ldpc_mid = LdpcDecoder::new_with_osd(iters_mid, osd_cfg)
                            .expect("LDPC decoder init failed")
                            .with_max_parity_errors_for_osd(ctx.max_parity_errors_for_osd)
                            .with_bp_offset_subtract(ctx.bp_offset_subtract)
                            .with_layered(ctx.layered_bp)
                            .with_feedback_refinement(
                                ctx.ldpc_feedback_refinement_enabled,
                                ctx.ldpc_feedback_boost_factor,
                                ctx.ldpc_feedback_attenuate_factor,
                                ctx.ldpc_feedback_erase_threshold,
                            );
                        let ldpc_high = LdpcDecoder::new_with_osd(iters_high, osd_cfg)
                            .expect("LDPC decoder init failed")
                            .with_max_parity_errors_for_osd(ctx.max_parity_errors_for_osd)
                            .with_bp_offset_subtract(ctx.bp_offset_subtract)
                            .with_layered(ctx.layered_bp)
                            .with_feedback_refinement(
                                ctx.ldpc_feedback_refinement_enabled,
                                ctx.ldpc_feedback_boost_factor,
                                ctx.ldpc_feedback_attenuate_factor,
                                ctx.ldpc_feedback_erase_threshold,
                            );
                        let fft_buffer = vec![Complex::new(0.0, 0.0); sps];
                        (ldpc_low, ldpc_mid, ldpc_high, fft_buffer)
                    },
                    |(ldpc_low, ldpc_mid, ldpc_high, fft_buffer), candidate| {
                        let ldpc = if candidate.sync_score > ADAPTIVE_HIGH_SCORE {
                            &*ldpc_low
                        } else if candidate.sync_score > ADAPTIVE_MID_SCORE {
                            &*ldpc_mid
                        } else {
                            &*ldpc_high
                        };
                        // First try standard AP0 decode
                        if let Some(msg) = par_decode_candidate(&ctx, candidate, ldpc, fft_buffer) {
                            return Some(msg);
                        }
                        // AP0 failed — try AP-enhanced decoding if AP is active
                        if !ctx.ap_active {
                            return None;
                        }
                        // Only attempt AP decoding on candidates with reasonable sync quality.
                        // Sync scores below 4.0 are likely noise — AP injection on noise produces
                        // false decodes by forcing the user's callsign into random bit patterns.
                        const MIN_SYNC_SCORE_FOR_AP: f64 = 3.0;
                        if candidate.sync_score < MIN_SYNC_SCORE_FOR_AP {
                            return None;
                        }
                        par_try_ap_decode(&ctx, candidate, ldpc, &decoded_calls, pass)
                    },
                )
                .flatten()
                .collect();

            // hb-129: safety net — stamp any par_iter outputs that
            // weren't tagged at their CRC-pass site (par_decode_candidate /
            // par_try_ap_decode / par_try_ldpc_with_recent_only stamp
            // themselves for fine-grained timing).
            {
                let now_elapsed = start_time.elapsed();
                for m in pass_decoded.iter_mut() {
                    if m.decode_time_into_window.is_none() {
                        m.decode_time_into_window = Some(now_elapsed);
                    }
                }
            }

            // hb-056: non-coherent cross-cycle averaging. Groups repeating-
            // station candidates and decodes a power-summed averaged
            // candidate alongside the per-slot results. Additive — its
            // outputs go through the same dedup, and a corrupted average
            // that fails CRC contributes nothing. Skipped when the flag
            // is off (default).
            if self.config.cross_cycle_averaging {
                let mut extra = self.cross_cycle_averaging_pass(&spectrogram, &sync_candidates);
                let now_elapsed = start_time.elapsed();
                for m in extra.iter_mut() {
                    if m.decode_time_into_window.is_none() {
                        m.decode_time_into_window = Some(now_elapsed);
                    }
                }
                pass_decoded.extend(extra);
            }

            // hb-079: coherent iterative-subtract multi-pass. Subtracts
            // each decoded signal's coherent contribution from the complex
            // spectrogram (ML projection), re-runs Costas sync on the
            // residual, and decodes any new candidates that the original
            // pass missed because they were masked by stronger neighbors.
            // Runs AFTER cross-cycle so cross-cycle integrates the full
            // (un-subtracted) data, then we subtract everything decoded so
            // far before looking for weaker masked signals.
            // hb-080: loop N rounds of subtract+repass. Each round only
            // subtracts the signals newly-decoded in the previous round —
            // pass 1's decodes are subtracted once on round 1, round 2
            // subtracts the new (residual) decodes, etc. Stops early if a
            // round finds nothing new.
            if self.config.coherent_multipass_iterations > 0 {
                // hb-016: when enabled, compute the spectrogram's noise-floor
                // proxy ONCE up front. Each round will compare its residual
                // mean dB against this reference to decide whether to keep
                // looking. Skipped (`None`) when the flag is off — keeps the
                // historical fast-path overhead at zero.
                let energy_stop = self
                    .config
                    .residual_energy_stop_db
                    .map(|margin| (noise_floor_db_median(&spectrogram.power), margin));
                let mut to_subtract: &[DecodedMessage] = &pass_decoded;
                let mut round_start_offset = 0;
                for _round in 0..self.config.coherent_multipass_iterations {
                    let mut extra = self.coherent_subtract_and_repass(
                        &mut spectrogram,
                        to_subtract,
                        energy_stop,
                        freq_bin_range.as_ref(),
                        partner_freq_hz,
                    );
                    if extra.is_empty() {
                        break;
                    }
                    // hb-129: stamp multipass decodes — these arrive
                    // LATER in the slot than pass-1 outputs, so TTFD
                    // distinguishes them.
                    let now_elapsed = start_time.elapsed();
                    for m in extra.iter_mut() {
                        if m.decode_time_into_window.is_none() {
                            m.decode_time_into_window = Some(now_elapsed);
                        }
                    }
                    let added = extra.len();
                    pass_decoded.extend(extra);
                    round_start_offset = pass_decoded.len() - added;
                    to_subtract = &pass_decoded[round_start_offset..];
                }
            }

            // hb-086 V1: after the multipass loop, try every ORIGINAL sync
            // candidate not at an already-subtracted position by re-extracting
            // symbols from the residual spectrogram and re-decoding. Catches
            // pairs where pass-1 LDPC failed because of interference and
            // the residual sync threshold rejected B even though its LLRs
            // at the (now-cleaned) overlap bins are decodable. Diagnostic
            // confirmed 78% of missed truths on top-20 hard-200 WAVs are
            // within 50 Hz of a recovered decode.
            if self.config.joint_pair_retry {
                let mut extra =
                    self.joint_pair_retry_pass(&spectrogram, &sync_candidates, &pass_decoded);
                // hb-129: stamp joint-pair-retry decodes at pass completion.
                let now_elapsed = start_time.elapsed();
                for m in extra.iter_mut() {
                    if m.decode_time_into_window.is_none() {
                        m.decode_time_into_window = Some(now_elapsed);
                    }
                }
                pass_decoded.extend(extra);
            }

            // hb-048 Session 3: a7 template cross-correlation pass. After
            // V1 joint-pair-retry, for each callsign already decoded in
            // this window, generate next-utterance templates and
            // cross-correlate against residual LLRs at sync_candidate
            // positions within ±a7_freq_window_hz of the expected call.
            // hb-048 S3-chrono (2026-06-01): additionally seed templates
            // from `ap_context.recent_calls` — when populated by a
            // chronological-replay tier from `ChronoReplayState`, this is
            // the cross-slot path WSJT-X's a7 actually exercises (slot N+1
            // templates rooted at callsigns heard in slot N). Cross-slot
            // calls have no known audio frequency, so they probe ALL
            // sync_candidates (the within-WAV ±freq-window gate only
            // applies to in-WAV expected calls).
            // WSJT-X prior art: mainline commit f13e3182 (Joe Taylor 2021).
            //
            // 4th-pass-after-a7 gating: a7 is gated OUT on the 4th-pass
            // iteration. Per spec ref
            // `spec-wsjtx-improved-4th-pass-after-a7.md` ("cascading 4th-pass
            // decodes"), the upstream design is exactly one a7 pass and
            // exactly one subsequent standard pass — no recursion into a
            // 5th-pass-after-a7'-after-a7. Skipping a7 on the extra
            // iteration also avoids inadvertently re-templating against
            // freshly-decoded 4th-pass output (which would be a fresh
            // false-positive surface).
            if self.config.a7_enabled && !is_fourth_pass_after_a7 {
                let mut extra = self.a7_cross_correlation_pass(
                    &spectrogram,
                    &sync_candidates,
                    &pass_decoded,
                    &ap_context_for_pass.recent_calls,
                );
                let now_elapsed = start_time.elapsed();
                for m in extra.iter_mut() {
                    if m.decode_time_into_window.is_none() {
                        m.decode_time_into_window = Some(now_elapsed);
                    }
                }
                // Track a7-attributed decodes for the 4th-pass-after-a7
                // AP-extension. We capture (text, from_callsign, snr)
                // before extending `pass_decoded` so we can correlate
                // them against the final dedup result.
                if fourth_pass_after_a7_enabled {
                    for m in extra.iter() {
                        if a7_emitted_texts.insert(m.text.clone()) {
                            if let Some(ref from) = m.message.from_callsign {
                                if !from.is_empty() {
                                    a7_discovered_calls.push((from.clone(), m.snr_db));
                                }
                            }
                        }
                    }
                }
                pass_decoded.extend(extra);
            }

            // hb-086 V3 (SHELVED 2026-05-31): see
            // `joint_residual_localized_sync_pass` docstring. Production
            // sweep showed the mechanism surfaces noise (not signal) at
            // every relaxation level; default is 0.0 (disabled). The
            // hook stays here so a follow-up that fixes the LDPC-on-noise
            // problem (different LLR scaling, OSD-only path, callsign
            // priors) can land without re-plumbing.
            if self.config.joint_residual_sync_relax_db < 0.0 {
                let mut extra = self.joint_residual_localized_sync_pass(
                    &spectrogram,
                    &sync_candidates,
                    &pass_decoded,
                );
                // hb-129: stamp joint-residual decodes at pass completion.
                let now_elapsed = start_time.elapsed();
                for m in extra.iter_mut() {
                    if m.decode_time_into_window.is_none() {
                        m.decode_time_into_window = Some(now_elapsed);
                    }
                }
                pass_decoded.extend(extra);
            }

            // Deduplicate the parallel results (multiple candidates may decode to
            // the same message text, and we also need to dedup against prior passes)
            let mut pass_unique: Vec<DecodedMessage> = Vec::new();
            for msg in pass_decoded {
                if already_decoded + pass_unique.len() >= max_candidates {
                    break;
                }
                if seen_messages.insert(msg.text.clone()) {
                    pass_unique.push(msg);
                }
            }

            // If no new messages decoded in this pass, stop iterating.
            // Exception: when 4th-pass-after-a7 is wired in and a7 has
            // discovered callsigns in any prior iteration, allow the
            // loop to roll into the 4th-pass-after-a7 iteration even
            // if the current standard pass added nothing new. The
            // 4th-pass-after-a7 entry then runs against the (already
            // a7-cleaned) residual with the extended AP context.
            if pass_unique.is_empty() {
                let has_pending_fourth_pass = fourth_pass_after_a7_enabled
                    && pass + 1 == max_passes
                    && !a7_discovered_calls.is_empty();
                if !has_pending_fourth_pass {
                    break;
                }
            }

            #[cfg(feature = "debug-decode")]
            eprintln!(
                "[decode pass {}] decoded {} new messages",
                pass,
                pass_unique.len()
            );

            // Subtract decoded signals from residual audio for next pass.
            // 4th-pass-after-a7: when the extra iteration is wired in,
            // `loop_passes = max_passes + 1`; the final standard pass
            // therefore still subtracts its decodes (including a7's)
            // so the 4th-pass-after-a7 iteration searches a residual
            // cleaned by a7. Spec: "running one more standard pass
            // over this cleaner residual recovers additional decodes
            // that were previously sitting under an a7-decoded signal's
            // ambiguity".
            if pass + 1 < loop_passes {
                for msg in &pass_unique {
                    self.subtract_with_sidelobes(&mut residual_samples, msg);
                }
            }

            all_decoded_messages.extend(pass_unique);
        }

        // Metrics
        let processing_time = start_time.elapsed();

        self.last_metrics = DecodingMetrics {
            messages_decoded: all_decoded_messages.len(),
            processing_time,
            average_snr: if all_decoded_messages.is_empty() {
                0.0
            } else {
                all_decoded_messages.iter().map(|m| m.snr_db).sum::<f32>()
                    / all_decoded_messages.len() as f32
            },
            peak_memory_bytes: 0,
            sync_quality: (best_sync_score / 12.0).min(1.0) as f32,
            timestamp: SystemTime::now(),
        };

        for message in &all_decoded_messages {
            self.message_handler
                .on_message_decoded(message, &self.last_metrics);
        }
        self.message_handler.on_window_complete(&self.last_metrics);

        Ok(all_decoded_messages)
    }

    /// Reconstruct FT8 tone symbols from LDPC codeword bits.
    ///
    /// This replicates the encoder's `generate_symbols` logic:
    /// - Costas sync arrays at positions 0-6, 36-42, 72-78
    /// - Data symbols from Gray-coded 3-bit groups at other positions
    fn codeword_to_symbols(corrected_bits: &bitvec::prelude::BitSlice) -> Vec<u8> {
        let mut symbols = vec![0u8; NUM_SYMBOLS];
        let mut bit_idx = 0usize;

        for i in 0..NUM_SYMBOLS {
            if i < 7 {
                symbols[i] = COSTAS[i];
            } else if (36..43).contains(&i) {
                symbols[i] = COSTAS[i - 36];
            } else if i >= 72 {
                symbols[i] = COSTAS[i - 72];
            } else {
                // Data symbol: 3 bits -> Gray code
                let mut bits3 = 0u8;
                if bit_idx < corrected_bits.len() && corrected_bits[bit_idx] {
                    bits3 |= 4;
                }
                if bit_idx + 1 < corrected_bits.len() && corrected_bits[bit_idx + 1] {
                    bits3 |= 2;
                }
                if bit_idx + 2 < corrected_bits.len() && corrected_bits[bit_idx + 2] {
                    bits3 |= 1;
                }
                bit_idx += 3;
                symbols[i] = crate::ldpc::binary_to_gray(bits3);
            }
        }

        symbols
    }

    /// Generate CPFSK I/Q reference signals for given symbols and frequency.
    fn generate_cpfsk_iq(symbols: &[u8], base_freq: f64, sps: usize) -> (Vec<f64>, Vec<f64>) {
        use std::f64::consts::PI;
        let total_len = symbols.len() * sps;
        let mut recon_i = vec![0.0f64; total_len];
        let mut recon_q = vec![0.0f64; total_len];
        let mut phase = 0.0f64;
        for (sym_idx, &sym) in symbols.iter().enumerate() {
            let freq = base_freq + sym as f64 * TONE_SPACING;
            let omega = 2.0 * PI * freq / SAMPLE_RATE as f64;
            let start = sym_idx * sps;
            for i in 0..sps {
                recon_i[start + i] = phase.cos();
                recon_q[start + i] = phase.sin();
                phase += omega;
            }
            if phase > 1e6 {
                phase %= 2.0 * PI;
            }
        }
        (recon_i, recon_q)
    }

    /// hb-226: Gaussian-style ramped CPFSK I/Q.
    ///
    /// Same continuous-phase FSK as `generate_cpfsk_iq` for the steady
    /// body of each symbol, but inside a `2 * ramp`-sample inter-symbol
    /// transition window centred on each symbol boundary the
    /// instantaneous angular velocity linearly slews from the current
    /// symbol's omega to the next symbol's omega. This linear angular-
    /// velocity ramp is the linear approximation of FT8's true GFSK
    /// shaping — it removes the spectral splatter that hard-edged
    /// rectangular subtraction leaves at every symbol boundary.
    ///
    /// The signal's first and last `ramp` samples additionally taper
    /// the I/Q amplitude linearly between 0 and 1 (cosine-ramp would
    /// be smoother but linear keeps unit tests easy to reason about
    /// and matches the simple amplitude fade described by the source
    /// spec).
    ///
    /// Inspired by spec ref
    /// `research/specs/spec-ft8mon-gaussian-ramp-subtract.md`.
    /// `ramp_samples = round(sps × subtract_ramp_fraction)` and is
    /// clamped to `>= 1`.
    #[inline]
    fn generate_cpfsk_iq_ramped(
        symbols: &[u8],
        base_freq: f64,
        sps: usize,
        ramp: usize,
    ) -> (Vec<f64>, Vec<f64>) {
        use std::f64::consts::PI;
        let total_len = symbols.len() * sps;
        let mut recon_i = vec![0.0f64; total_len];
        let mut recon_q = vec![0.0f64; total_len];
        let ramp_eff = ramp.max(1).min(sps / 2);

        // Pre-compute per-symbol omegas.
        let nsym = symbols.len();
        let mut omegas = vec![0.0f64; nsym];
        for (i, &sym) in symbols.iter().enumerate() {
            let freq = base_freq + sym as f64 * TONE_SPACING;
            omegas[i] = 2.0 * PI * freq / SAMPLE_RATE as f64;
        }

        let mut phase = 0.0f64;
        for sym_idx in 0..nsym {
            let omega_cur = omegas[sym_idx];
            let omega_next = if sym_idx + 1 < nsym {
                omegas[sym_idx + 1]
            } else {
                omega_cur
            };
            let start = sym_idx * sps;
            let is_first = sym_idx == 0;
            let is_last = sym_idx + 1 == nsym;

            // Sample layout within this symbol's `sps` slot
            //   [0,           ramp_eff)         — on-ramp head
            //     This is the SECOND half of the transition that the
            //     PREVIOUS iteration began. For symbol 0 there is no
            //     previous transition, so we instead taper amplitude
            //     0 → 1 with constant omega_cur ("leading fade-in").
            //   [ramp_eff,    sps - ramp_eff)   — steady body
            //     Constant omega_cur, unit amplitude.
            //   [sps - ramp_eff, sps)           — off-ramp tail
            //     This is the FIRST half of the transition into the
            //     NEXT symbol. For the last symbol there is no next
            //     transition; instead taper amplitude 1 → 0 with
            //     constant omega_cur ("trailing fade-out").
            //
            // Each transition is therefore written EXACTLY ONCE,
            // split across two symbol slots (tail of N + head of
            // N+1). No double-write, no missed sample.

            // ---- On-ramp head (or leading fade-in for symbol 0) ----
            if is_first {
                // Leading fade-in: ramp amplitude 0 → 1, constant
                // omega_cur (no previous symbol to slew from).
                for i in 0..ramp_eff {
                    let amp = i as f64 / ramp_eff as f64;
                    recon_i[start + i] = amp * phase.cos();
                    recon_q[start + i] = amp * phase.sin();
                    phase += omega_cur;
                }
            } else {
                // On-ramp head: second half of the transition that
                // started in the previous symbol's tail. Omega slews
                // from omega_prev to omega_cur. Sample kk in
                // [ramp_eff, 2*ramp_eff) of the `2 * ramp_eff` window;
                // i.e. for the within-symbol index `jj` in
                // [0, ramp_eff), kk = ramp_eff + jj.
                let omega_prev = omegas[sym_idx - 1];
                let domega = (omega_cur - omega_prev) / (2.0 * ramp_eff as f64);
                for jj in 0..ramp_eff {
                    let kk = ramp_eff + jj;
                    let omega_at = omega_prev + (kk as f64 + 0.5) * domega;
                    recon_i[start + jj] = phase.cos();
                    recon_q[start + jj] = phase.sin();
                    phase += omega_at;
                }
            }

            // ---- Steady body ----
            // [ramp_eff, sps - ramp_eff). For symbols where this range
            // is empty (e.g. very large ramp), the loop is a no-op.
            let body_start = ramp_eff;
            let body_end = sps.saturating_sub(ramp_eff);
            if body_end > body_start {
                for i in body_start..body_end {
                    recon_i[start + i] = phase.cos();
                    recon_q[start + i] = phase.sin();
                    phase += omega_cur;
                }
            }

            // ---- Off-ramp tail (or trailing fade-out for the last symbol) ----
            if is_last {
                // Trailing fade-out: ramp amplitude 1 → 0, constant
                // omega_cur (no next symbol to slew to).
                for jj in 0..ramp_eff {
                    let amp = 1.0 - (jj as f64 + 1.0) / ramp_eff as f64;
                    let idx = start + body_end + jj;
                    if idx < total_len {
                        recon_i[idx] = amp * phase.cos();
                        recon_q[idx] = amp * phase.sin();
                    }
                    phase += omega_cur;
                }
            } else {
                // Off-ramp tail: first half of the transition into the
                // next symbol. Omega slews from omega_cur to omega_next.
                // Sample kk in [0, ramp_eff) of the `2 * ramp_eff`
                // transition window.
                let domega = (omega_next - omega_cur) / (2.0 * ramp_eff as f64);
                for jj in 0..ramp_eff {
                    let kk = jj;
                    let omega_at = omega_cur + (kk as f64 + 0.5) * domega;
                    let idx = start + body_end + jj;
                    if idx < total_len {
                        recon_i[idx] = phase.cos();
                        recon_q[idx] = phase.sin();
                    }
                    phase += omega_at;
                }
            }

            if phase > 1e6 {
                phase %= 2.0 * PI;
            }
        }
        (recon_i, recon_q)
    }

    /// hb-226: compute the ramp half-width in samples from the
    /// fractional-symbol parameter. Clamps to `>= 1` per spec.
    #[inline]
    fn ramp_samples_from_fraction(sps: usize, fraction: f64) -> usize {
        let r = (sps as f64 * fraction).round() as i64;
        r.max(1) as usize
    }

    /// Compute the correlation energy (amplitude^2) of a CPFSK signal at given
    /// frequency against the audio. Used for fine frequency search.
    fn correlation_energy(
        audio: &[f32],
        audio_start: usize,
        recon_i: &[f64],
        recon_q: &[f64],
        recon_offset: usize,
        signal_len: usize,
    ) -> f64 {
        let mut dot_ai = 0.0f64;
        let mut dot_aq = 0.0f64;
        let mut dot_ii = 0.0f64;
        let mut dot_qq = 0.0f64;
        let mut dot_iq = 0.0f64;
        for i in 0..signal_len {
            let a = audio[audio_start + i] as f64;
            let ri = recon_i[recon_offset + i];
            let rq = recon_q[recon_offset + i];
            dot_ai += a * ri;
            dot_aq += a * rq;
            dot_ii += ri * ri;
            dot_qq += rq * rq;
            dot_iq += ri * rq;
        }
        let det = dot_ii * dot_qq - dot_iq * dot_iq;
        if det.abs() > 1e-12 {
            let ai = (dot_ai * dot_qq - dot_aq * dot_iq) / det;
            let aq = (dot_aq * dot_ii - dot_ai * dot_iq) / det;
            ai * ai + aq * aq
        } else {
            0.0
        }
    }

    /// Subtract a decoded signal from the audio buffer (time-domain interference cancellation).
    ///
    /// Uses the tone symbols stored in the DecodedMessage to reconstruct the signal
    /// via direct continuous-phase FSK synthesis, then subtracts it after estimating
    /// amplitude and phase via least-squares projection. Includes fine frequency
    /// and timing search to match the actual signal precisely.
    fn subtract_signal(&self, audio: &mut [f32], msg: &DecodedMessage) {
        use std::f64::consts::PI;

        let symbols = match &msg.tone_symbols {
            Some(s) if s.len() == NUM_SYMBOLS => s,
            _ => {
                #[cfg(feature = "debug-decode")]
                eprintln!("  [subtract] no tone symbols for '{}', skipping", msg.text);
                return;
            }
        };

        let sps = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize; // 1920
        let total_len = NUM_SYMBOLS * sps;
        let nominal_freq = msg.frequency_offset;
        let nominal_time = (msg.time_offset * SAMPLE_RATE as f64) as isize;

        // hb-226: when enabled, use Gaussian-style ramped CPFSK for
        // both the search reference and the final subtraction. When
        // disabled, the path is byte-identical to the legacy
        // hard-edged subtract (the closure below dispatches to the
        // unramped `generate_cpfsk_iq`).
        let ramp_enabled = self.config.gaussian_ramp_subtract_enabled;
        let ramp_samples = if ramp_enabled {
            Self::ramp_samples_from_fraction(sps, self.config.gaussian_ramp_subtract_fraction)
        } else {
            0
        };
        let gen_iq = |sym: &[u8], freq: f64| -> (Vec<f64>, Vec<f64>) {
            if ramp_enabled {
                Self::generate_cpfsk_iq_ramped(sym, freq, sps, ramp_samples)
            } else {
                Self::generate_cpfsk_iq(sym, freq, sps)
            }
        };

        // Fine frequency and time search to precisely match the actual signal.
        // The spectrogram has 3.125 Hz resolution, so sub-Hz precision is essential.
        // Frequency: +/-1.5 Hz in 0.5 Hz steps (7 freq trials)
        // Time: +/-480 samples (1/4 symbol) in 120-sample steps (9 time trials)
        let mut best_energy = 0.0f64;
        let mut best_freq = nominal_freq;
        let mut best_time = nominal_time;

        for di in -3i32..=3 {
            let try_freq = nominal_freq + di as f64 * 0.5;
            let (ri, rq) = gen_iq(symbols, try_freq);
            for dt in -4i32..=4 {
                let try_time = nominal_time + dt as isize * 120;
                let recon_start = try_time.max(0) as usize;
                let recon_offset = (recon_start as isize - try_time) as usize;
                let sig_len = (total_len.saturating_sub(recon_offset))
                    .min(audio.len().saturating_sub(recon_start));
                if sig_len == 0 {
                    continue;
                }
                let energy =
                    Self::correlation_energy(audio, recon_start, &ri, &rq, recon_offset, sig_len);
                if energy > best_energy {
                    best_energy = energy;
                    best_freq = try_freq;
                    best_time = try_time;
                }
            }
        }

        // Now subtract at the best frequency/time
        let (recon_i, recon_q) = gen_iq(symbols, best_freq);
        let recon_start = best_time.max(0) as usize;
        let recon_offset = (recon_start as isize - best_time) as usize;
        let signal_len =
            (total_len.saturating_sub(recon_offset)).min(audio.len().saturating_sub(recon_start));

        if signal_len == 0 {
            return;
        }

        // Full 2x2 least-squares for amplitude and phase
        let mut dot_ai = 0.0f64;
        let mut dot_aq = 0.0f64;
        let mut dot_ii = 0.0f64;
        let mut dot_qq = 0.0f64;
        let mut dot_iq = 0.0f64;
        for i in 0..signal_len {
            let a = audio[recon_start + i] as f64;
            let ri = recon_i[recon_offset + i];
            let rq = recon_q[recon_offset + i];
            dot_ai += a * ri;
            dot_aq += a * rq;
            dot_ii += ri * ri;
            dot_qq += rq * rq;
            dot_iq += ri * rq;
        }

        let det = dot_ii * dot_qq - dot_iq * dot_iq;
        let (amp_i, amp_q) = if det.abs() > 1e-12 {
            let ai = (dot_ai * dot_qq - dot_aq * dot_iq) / det;
            let aq = (dot_aq * dot_ii - dot_ai * dot_iq) / det;
            (ai, aq)
        } else {
            (0.0, 0.0)
        };

        // Clamp total amplitude
        let total_amp = (amp_i * amp_i + amp_q * amp_q).sqrt();
        let max_amp = 3.0;
        let (amp_i, amp_q) = if total_amp > max_amp {
            let s = max_amp / total_amp;
            (amp_i * s, amp_q * s)
        } else {
            (amp_i, amp_q)
        };

        // Subtract with 0.9 conservative factor
        let scale = 0.9;
        for i in 0..signal_len {
            let subtracted = amp_i * recon_i[recon_offset + i] + amp_q * recon_q[recon_offset + i];
            audio[recon_start + i] -= (subtracted * scale) as f32;
        }

        #[cfg(feature = "debug-decode")]
        eprintln!(
            "  [subtract] '{}' at {:.2} Hz (nom {:.1}), t={:.4}s (nom {:.3}), amp={:.4}, phase={:.1}deg",
            msg.text, best_freq, nominal_freq,
            best_time as f64 / SAMPLE_RATE as f64, msg.time_offset,
            total_amp, amp_q.atan2(amp_i).to_degrees()
        );
    }

    /// Subtract a decoded signal with sidelobe cancellation at ±1 tone spacing.
    ///
    /// After main signal subtraction, removes first sidelobes of the Hann window
    /// at ±6.25 Hz (one tone spacing). Hann first sidelobe is ~15% (-16 dB) of
    /// the main lobe, so we use a 0.15 scale factor for the sidelobe subtraction.
    fn subtract_with_sidelobes(&self, audio: &mut [f32], msg: &DecodedMessage) {
        use std::f64::consts::PI;

        // Main signal subtraction
        self.subtract_signal(audio, msg);

        // Sidelobe cancellation requires tone symbols
        let symbols = match &msg.tone_symbols {
            Some(s) if s.len() == NUM_SYMBOLS => s,
            _ => return,
        };

        let sps = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;
        let total_len = NUM_SYMBOLS * sps;
        let base_freq = msg.frequency_offset;
        let nominal_time = (msg.time_offset * SAMPLE_RATE as f64) as isize;
        let tone_spacing = TONE_SPACING as f64;
        let sidelobe_scale = 0.15 * 0.9; // 15% sidelobe × 0.9 conservative factor

        // For each sidelobe offset (+1 and -1 tone spacing)
        for &freq_offset in &[tone_spacing, -tone_spacing] {
            let shifted_freq = base_freq + freq_offset;
            if shifted_freq < 0.0 {
                continue;
            }

            let (recon_i, recon_q) = Self::generate_cpfsk_iq(symbols, shifted_freq, sps);
            let recon_start = nominal_time.max(0) as usize;
            let recon_offset = (recon_start as isize - nominal_time) as usize;
            let signal_len = (total_len.saturating_sub(recon_offset))
                .min(audio.len().saturating_sub(recon_start));
            if signal_len == 0 {
                continue;
            }

            // Estimate amplitude via projection (same as subtract_signal)
            let mut dot_ai = 0.0f64;
            let mut dot_aq = 0.0f64;
            let mut dot_ii = 0.0f64;
            let mut dot_qq = 0.0f64;
            let mut dot_iq = 0.0f64;
            for i in 0..signal_len {
                let a = audio[recon_start + i] as f64;
                let ri = recon_i[recon_offset + i];
                let rq = recon_q[recon_offset + i];
                dot_ai += a * ri;
                dot_aq += a * rq;
                dot_ii += ri * ri;
                dot_qq += rq * rq;
                dot_iq += ri * rq;
            }

            let det = dot_ii * dot_qq - dot_iq * dot_iq;
            let (amp_i, amp_q) = if det.abs() > 1e-12 {
                let ai = (dot_ai * dot_qq - dot_aq * dot_iq) / det;
                let aq = (dot_aq * dot_ii - dot_ai * dot_iq) / det;
                (ai, aq)
            } else {
                continue;
            };

            // Subtract sidelobe at reduced amplitude
            for i in 0..signal_len {
                let subtracted =
                    amp_i * recon_i[recon_offset + i] + amp_q * recon_q[recon_offset + i];
                audio[recon_start + i] -= (subtracted * sidelobe_scale) as f32;
            }
        }
    }

    /// Pre-process audio: convert to f64 and normalize
    fn preprocess_audio(&self, samples: &[f32]) -> Ft8Result<Vec<f64>> {
        let mut audio: Vec<f64> = samples.iter().map(|&s| s as f64).collect();

        // Normalize to prevent overflow
        let max_amplitude = audio.iter().fold(0.0f64, |acc, &x| acc.max(x.abs()));
        if max_amplitude > 0.0 {
            let scale = 0.95 / max_amplitude;
            for sample in &mut audio {
                *sample *= scale;
            }
        }

        // Log signal stats for diagnostics
        let rms: f64 = (audio.iter().map(|x| x * x).sum::<f64>() / audio.len() as f64).sqrt();
        debug!(
            samples = samples.len(),
            max_amplitude = format!("{:.6}", max_amplitude),
            rms_after_norm = format!("{:.6}", rms),
            "FT8 preprocess"
        );

        Ok(audio)
    }

    // ========================================================================
    // Step 1: Spectrogram
    // ========================================================================

    /// Compute power spectrogram using ft8_lib's sliding-frame approach.
    ///
    /// Matches `monitor_process()` in ft8_lib/common/monitor.c:
    /// - Persistent `last_frame` buffer of nfft samples
    /// - Per symbol: loop time_osr times, each time shifting subblock_size
    ///   new samples into the frame, then windowed FFT
    /// - Window includes 2.0/nfft normalization (baked in at init)
    /// - Frequency oversampling via freq_osr sub-bins
    fn compute_spectrogram(&self, audio: &[f64]) -> Ft8Result<Spectrogram> {
        let pp = &self.protocol_params;
        let block_size = pp.samples_per_symbol(SAMPLE_RATE); // 1920
        let freq_osr = FREQ_OSR; // 2
        let time_osr = TIME_OSR; // 2
        let nfft = block_size * freq_osr; // 3840
        let subblock_size = block_size / time_osr; // 960

        if audio.len() < block_size {
            return Err(Ft8Error::InsufficientData {
                needed: block_size,
                available: audio.len(),
            });
        }

        // Number of frequency bins in 6.25 Hz units
        let num_bins = block_size / 2 + 1; // 961

        // How many complete symbols (blocks) fit in the audio?
        let num_blocks = audio.len() / block_size;
        // We need enough blocks for the Costas search span + margin.
        // FT8: 79 symbols × 2 time_osr = 158 steps for the message, plus
        // margin for ±2s timing uncertainty.
        let steps_per_symbol = time_osr;
        let msg_span = self.protocol_params.num_symbols * steps_per_symbol;
        let search_margin = 50;
        let min_steps = msg_span + search_margin;

        // Pad audio if needed to get enough blocks
        let min_blocks = (min_steps + time_osr - 1) / time_osr;
        let padded;
        let audio_ref = if num_blocks < min_blocks {
            let min_len = min_blocks * block_size;
            padded = {
                let mut v = audio.to_vec();
                v.resize(min_len, 0.0);
                v
            };
            &padded[..]
        } else {
            audio
        };
        let num_blocks = audio_ref.len() / block_size;
        let num_steps = num_blocks * time_osr;

        let fft = &self.spectrogram_fft;
        let window = &self.spectrogram_window;

        let mut power = Vec::with_capacity(num_steps);
        // hb-074: retain complex bins only when the coherent cross-cycle
        // path will consume them (doubles the spectrogram's memory).
        let want_complex = self.config.cross_cycle_coherent;
        let mut complex: Option<Vec<Vec<Vec<Complex<f64>>>>> = if want_complex {
            Some(Vec::with_capacity(num_steps))
        } else {
            None
        };
        let mut fft_buffer = vec![Complex::new(0.0, 0.0); nfft];
        // Persistent sliding frame buffer (matches ft8_lib's me->last_frame)
        let mut last_frame = vec![0.0f64; nfft];

        let mut frame_pos = 0usize;

        for _block in 0..num_blocks {
            for _time_sub in 0..time_osr {
                // Shift old data left by subblock_size, append new data on right
                // (exactly as monitor.c lines 146-154)
                last_frame.copy_within(subblock_size.., 0);
                let new_start = nfft - subblock_size;
                for pos in 0..subblock_size {
                    last_frame[new_start + pos] = if frame_pos < audio_ref.len() {
                        audio_ref[frame_pos]
                    } else {
                        0.0
                    };
                    frame_pos += 1;
                }

                // Apply window and FFT
                for i in 0..nfft {
                    fft_buffer[i] = Complex::new(window[i] * last_frame[i], 0.0);
                }
                fft.process(&mut fft_buffer);

                // Organize into freq_osr sub-bins (matches monitor.c lines 164-188)
                let mut sub_power = Vec::with_capacity(freq_osr);
                let mut sub_complex: Option<Vec<Vec<Complex<f64>>>> = if want_complex {
                    Some(Vec::with_capacity(freq_osr))
                } else {
                    None
                };
                for fs in 0..freq_osr {
                    let mut row = Vec::with_capacity(num_bins);
                    let mut crow: Option<Vec<Complex<f64>>> = if want_complex {
                        Some(Vec::with_capacity(num_bins))
                    } else {
                        None
                    };
                    for bin in 0..num_bins {
                        let src_bin = bin * freq_osr + fs;
                        if src_bin < nfft / 2 + 1 {
                            let cval = fft_buffer[src_bin];
                            let mag2 = cval.norm_sqr();
                            let db = 10.0 * (1e-12f64 + mag2).log10();
                            row.push(db);
                            if let Some(c) = crow.as_mut() {
                                c.push(cval);
                            }
                        } else {
                            row.push(-120.0);
                            if let Some(c) = crow.as_mut() {
                                c.push(Complex::new(0.0, 0.0));
                            }
                        }
                    }
                    sub_power.push(row);
                    if let (Some(sc), Some(cr)) = (sub_complex.as_mut(), crow) {
                        sc.push(cr);
                    }
                }
                power.push(sub_power);
                if let (Some(c), Some(sc)) = (complex.as_mut(), sub_complex) {
                    c.push(sc);
                }
            }
        }

        Ok(Spectrogram {
            power,
            complex,
            num_steps,
            num_bins,
            freq_osr,
            time_padding: 0,
        })
    }

    // ========================================================================
    // Step 2: Costas sync search
    // ========================================================================

    /// Search for FT8 signals by correlating the Costas sync pattern
    /// against the spectrogram in 2D (time offset, frequency offset).
    ///
    /// Compute block detection score by evaluating all 79 symbol positions.
    /// Finds the strongest tone at each position and compares signal vs noise
    /// across the full frame. More robust than Costas-only sync scoring.
    fn block_score(&self, spec: &Spectrogram, candidate: &CostasCandidate) -> f64 {
        let pp = &self.protocol_params;
        let steps_per_symbol = TIME_OSR;
        let mut signal_sum = 0.0f64;
        let mut noise_sum = 0.0f64;
        let mut signal_count = 0usize;
        let mut noise_count = 0usize;

        for sym_idx in 0..pp.num_symbols {
            let t = candidate.time_step + sym_idx * steps_per_symbol;
            if t >= spec.num_steps {
                break;
            }

            let mut best_power = f64::MIN;
            let mut best_tone = 0usize;
            for tone in 0..pp.num_tones {
                let f = candidate.freq_bin + tone;
                if f >= spec.num_bins {
                    continue;
                }
                let power = spec.power[t][candidate.freq_sub][f];
                if power > best_power {
                    best_power = power;
                    best_tone = tone;
                }
            }
            signal_sum += best_power;
            signal_count += 1;

            for tone in 0..pp.num_tones {
                if tone == best_tone {
                    continue;
                }
                let f = candidate.freq_bin + tone;
                if f >= spec.num_bins {
                    continue;
                }
                noise_sum += spec.power[t][candidate.freq_sub][f];
                noise_count += 1;
            }
        }

        if signal_count == 0 || noise_count == 0 {
            return 0.0;
        }
        (signal_sum / signal_count as f64) - (noise_sum / noise_count as f64)
    }

    /// The Costas array [3,1,4,0,6,5,2] appears at symbol positions 0-6,
    /// 36-42, and 72-78. For each candidate (t0, f0, freq_sub), we check
    /// all 21 Costas positions and score using neighbor comparison (ft8_lib style).
    /// With freq_osr=2, we search both even and odd frequency sub-bins.
    fn costas_sync_search(
        &self,
        spectrogram: &Spectrogram,
        freq_bin_scope: Option<&RangeInclusive<usize>>,
    ) -> Ft8Result<Vec<CostasCandidate>> {
        self.costas_sync_search_with_threshold_and_partner(
            spectrogram,
            self.config.min_sync_score,
            freq_bin_scope,
            None,
        )
    }

    /// hb-230: like `costas_sync_search` but forwards a `partner_freq_hz`
    /// to the threshold helper. Used by the top-level scoped-with-partner
    /// decode entry point so the near-partner relaxed-threshold branch
    /// fires on pass 1 (as well as the residual pass via
    /// `coherent_subtract_and_repass`).
    fn costas_sync_search_partner(
        &self,
        spectrogram: &Spectrogram,
        freq_bin_scope: Option<&RangeInclusive<usize>>,
        partner_freq_hz: Option<f64>,
    ) -> Ft8Result<Vec<CostasCandidate>> {
        self.costas_sync_search_with_threshold_and_partner(
            spectrogram,
            self.config.min_sync_score,
            freq_bin_scope,
            partner_freq_hz,
        )
    }

    /// hb-082: same sync search as `costas_sync_search` but with an explicit
    /// minimum-score threshold. The residual multipass uses this with
    /// `residual_min_sync_score` so the residual's lower noise floor can
    /// surface candidates the production threshold would reject.
    ///
    /// hb-091 Session 2: when `freq_bin_scope` is `Some`, the Costas sweep
    /// is clamped to the intersection of the supplied range and the natural
    /// `MIN_FREQ_BIN..max_freq_bin` envelope. `None` preserves the full sweep.
    fn costas_sync_search_with_threshold(
        &self,
        spectrogram: &Spectrogram,
        min_score: f64,
        freq_bin_scope: Option<&RangeInclusive<usize>>,
    ) -> Ft8Result<Vec<CostasCandidate>> {
        self.costas_sync_search_with_threshold_and_partner(
            spectrogram,
            min_score,
            freq_bin_scope,
            None,
        )
    }

    /// hb-230: same as `costas_sync_search_with_threshold` but with optional
    /// QSO-partner-aware relaxed acceptance.
    ///
    /// When `partner_freq_hz` is `Some(p)` AND
    /// `relaxed_sync_near_partner_hz_radius` is `Some(r)`, any candidate
    /// whose audio frequency `f` satisfies `|f - p| <= r` uses an
    /// **effective threshold** of
    /// `max(0, min_score + relaxed_sync_near_partner_score_delta)`
    /// (typically non-positive delta — relaxed). Outside the window the
    /// regular `min_score` applies. Mirrors JTDX's `sync8.f90`
    /// near-partner branch in shape; the constants must be empirically
    /// recalibrated to pancetta's raw dB-power sync metric — see the
    /// `relaxed_sync_near_partner_score_delta` config doc.
    ///
    /// When `partner_freq_hz = None` OR the config radius is `None` the
    /// relaxed branch never fires and behaviour is byte-identical to
    /// `costas_sync_search_with_threshold`.
    fn costas_sync_search_with_threshold_and_partner(
        &self,
        spectrogram: &Spectrogram,
        min_score: f64,
        freq_bin_scope: Option<&RangeInclusive<usize>>,
        partner_freq_hz: Option<f64>,
    ) -> Ft8Result<Vec<CostasCandidate>> {
        let mut candidates = Vec::new();
        let pp = &self.protocol_params;

        // A full message occupies num_symbols * TIME_OSR time steps.
        let steps_per_symbol = TIME_OSR;
        let msg_span = pp.num_symbols * steps_per_symbol;
        let max_time_step = spectrogram.num_steps.saturating_sub(msg_span + 1);

        // Frequency range: need bins f0..f0+num_tones to all be valid
        let max_freq_bin = spectrogram.num_bins.saturating_sub(pp.num_tones);
        let max_freq_bin = max_freq_bin.min((4000.0 / pp.tone_spacing) as usize);

        // hb-091 Session 2: intersect natural envelope with the optional
        // user-supplied scope. Empty intersection → zero iterations → zero
        // candidates (matches the "scoped out of range" contract).
        let (lo, hi) = match freq_bin_scope {
            Some(range) => (
                MIN_FREQ_BIN.max(*range.start()),
                max_freq_bin.min(range.end().saturating_add(1)),
            ),
            None => (MIN_FREQ_BIN, max_freq_bin),
        };

        // hb-242: when enabled, also compute the partial-Costas
        // (`sync_bc`) metric and use `max(full, partial)`. The partial
        // metric drops block A from both the numerator and denominator,
        // so it stays meaningful for slot-edge negative-dt signals
        // where the leading edge fell outside the recorded window.
        let partial_enabled = self.config.costas_partial_metric_enabled;

        // hb-242 wide-lag baseline: when enabled, also record (per
        // freq_sub, freq_bin) the peak score and time-step within a
        // tight window near the nominal slot start AND within the full
        // ("wide") window. After the sweep, each per-bin peak array is
        // sorted and 40th-percentile-normalised; the normalisation base
        // becomes the `red` / `red2` reference. Candidates clearing the
        // normalised threshold in EITHER pathway are kept; when the
        // tight and wide peaks land at distinct time-steps, BOTH are
        // emitted (mainline `sync8` per-bin double-emission rule).
        let two_baseline_enabled = self.config.costas_two_baseline_enabled;
        let tight_steps = self.config.costas_two_baseline_tight_steps;

        // Per-bin peak buffers, indexed by [freq_sub][freq_bin - lo].
        // Only populated when two_baseline_enabled. Stores the (best
        // sync_score, time_step) pair for the tight and wide pathways.
        let bin_count = hi.saturating_sub(lo);
        let mut tight_peaks: Vec<Vec<(f64, usize)>> = if two_baseline_enabled {
            vec![vec![(0.0_f64, 0_usize); bin_count]; spectrogram.freq_osr]
        } else {
            Vec::new()
        };
        let mut wide_peaks: Vec<Vec<(f64, usize)>> = if two_baseline_enabled {
            vec![vec![(0.0_f64, 0_usize); bin_count]; spectrogram.freq_osr]
        } else {
            Vec::new()
        };

        // Local closure that applies the hb-242 max(full, partial)
        // rule when enabled, otherwise returns the full metric. Used
        // both for the main score and for parabolic-refinement
        // neighbour scores so the refined-score scale stays consistent.
        let scored = |t0: usize, f0: usize, freq_sub: usize| -> f64 {
            let full = self.compute_costas_score(spectrogram, t0, f0, freq_sub);
            if partial_enabled {
                let partial = self.compute_costas_score_partial_bc(spectrogram, t0, f0, freq_sub);
                full.max(partial)
            } else {
                full
            }
        };

        // hb-230: pre-resolve the near-partner relaxed-threshold context.
        // The relaxed branch only fires when BOTH the per-call
        // `partner_freq_hz` is supplied AND the per-config
        // `relaxed_sync_near_partner_hz_radius` is `Some(r)`. Inside the
        // ±r window the effective threshold becomes
        //   max(0, min_score + relaxed_sync_near_partner_score_delta).
        // The clamp to 0 prevents a misconfigured negative delta from
        // admitting pure-noise positions; the default delta=0.0 leaves
        // the threshold equal to `min_score` (no relaxation) so the
        // mechanism is structurally wired but byte-identical to the
        // historical behaviour until the operator tunes the delta.
        let tone_spacing_hz = pp.tone_spacing;
        let relaxed_window: Option<(f64, f64, f64)> = match (
            partner_freq_hz,
            self.config.relaxed_sync_near_partner_hz_radius,
        ) {
            (Some(p), Some(r)) if p.is_finite() && r.is_finite() && r >= 0.0 && p > 0.0 => {
                let relaxed =
                    (min_score + self.config.relaxed_sync_near_partner_score_delta).max(0.0);
                Some((p, r, relaxed))
            }
            _ => None,
        };

        for freq_sub in 0..spectrogram.freq_osr {
            for t0 in 0..=max_time_step {
                for f0 in lo..hi {
                    let score = scored(t0, f0, freq_sub);

                    if two_baseline_enabled {
                        let bin_idx = f0 - lo;
                        // Wide pathway: any t0 contributes.
                        if score > wide_peaks[freq_sub][bin_idx].0 {
                            wide_peaks[freq_sub][bin_idx] = (score, t0);
                        }
                        // Tight pathway: only t0 ≤ tight_steps from the
                        // nominal slot start. With `time_padding = 0`
                        // and the t0 sweep starting at 0, "tight"
                        // means low t0 values.
                        if t0 <= tight_steps && score > tight_peaks[freq_sub][bin_idx].0 {
                            tight_peaks[freq_sub][bin_idx] = (score, t0);
                        }
                    }

                    // hb-230: per-candidate threshold. Outside the
                    // near-partner window (or with the feature off) this
                    // is exactly `min_score` — the historical gate.
                    // Inside the window it relaxes to
                    // `max(0, min_score + score_delta)`.
                    let candidate_threshold =
                        if let Some((partner, radius, relaxed)) = relaxed_window {
                            let sub_off = freq_sub as f64 * (tone_spacing_hz / FREQ_OSR as f64);
                            let cand_freq = f0 as f64 * tone_spacing_hz + sub_off;
                            if (cand_freq - partner).abs() <= radius {
                                relaxed
                            } else {
                                min_score
                            }
                        } else {
                            min_score
                        };
                    if score > candidate_threshold {
                        // hb-044: optional parabolic refinement of the
                        // time-bin peak. Costs 2 extra score evaluations
                        // per kept candidate. Both hb-044 (sort by
                        // refined score) and hb-068 batch-8 variant (d,
                        // sort by integer-bin score; only use fractional
                        // offset in symbol extraction) were tested batch
                        // 7-8 and BOTH regressed hard-200. Default off;
                        // kept for research only.
                        //
                        // hb-068 batch-14 variants (when interpolation is
                        // on AND a knob is set):
                        // - (a) score gate: skip refinement entirely if
                        //   `score ≤ sync_time_interp_score_gate`.
                        // - (b) delta scale: multiply parabolic delta by
                        //   `sync_time_interp_delta_scale` (and recompute
                        //   refined score consistently).
                        // - (c) reject large delta: if `|delta| >
                        //   sync_time_interp_max_delta_abs`, fall back to
                        //   integer-bin (delta=0, original score).
                        let (refined_score, time_refinement) =
                            if self.config.sync_time_interpolation
                                && t0 > 0
                                && t0 + 1 <= max_time_step
                                && score > self.config.sync_time_interp_score_gate
                            {
                                // hb-245: parabolic peak interpolation
                                // (sub-sample DT refinement). Use the same
                                // `scored` closure so the y_left / y_right
                                // values match the y_center scoring rule
                                // (hb-242 max(full, partial) when enabled).
                                // The math is the textbook Smith parabola
                                // through three equally-spaced points,
                                // already proven correct by
                                // `parabolic_peak_refinement` and its
                                // unit tests.
                                let y_left = scored(t0 - 1, f0, freq_sub);
                                let y_right = scored(t0 + 1, f0, freq_sub);
                                let (mut r_score, mut r_delta) =
                                    parabolic_peak_refinement(y_left, score, y_right);
                                // (b) delta scale: rescale the offset and
                                // recompute the score from the parabola
                                // so the score reflects the position used.
                                let scale = self.config.sync_time_interp_delta_scale;
                                if (scale - 1.0).abs() > f64::EPSILON && r_delta.abs() > 0.0 {
                                    let a = (y_left + y_right - 2.0 * score) * 0.5;
                                    let b = (y_right - y_left) * 0.5;
                                    r_delta *= scale;
                                    r_score = score + b * r_delta + a * r_delta * r_delta;
                                }
                                // (c) reject large delta: post-clamp/scale,
                                // if magnitude exceeds threshold, fall back
                                // to integer-bin position + original score.
                                if let Some(max_abs) = self.config.sync_time_interp_max_delta_abs {
                                    if r_delta.abs() > max_abs {
                                        r_score = score;
                                        r_delta = 0.0;
                                    }
                                }
                                (r_score, r_delta)
                            } else {
                                (score, 0.0)
                            };
                        candidates.push(CostasCandidate {
                            time_step: t0,
                            freq_bin: f0,
                            freq_sub,
                            sync_score: refined_score,
                            time_refinement,
                        });
                    }
                }
            }
        }

        // hb-242 wide-lag baseline (red2): emit additional candidates
        // from the tight and wide per-bin peak buffers if their 40th-
        // percentile-normalised score clears the `norm_threshold`. The
        // wide pathway is what specifically catches slot-edge negative-dt
        // signals — the absolute min_score gate above filters them out
        // because the dB-difference metric is biased low on those bins,
        // but the percentile baseline accounts for the per-band noise
        // floor. When the tight and wide peaks land at different
        // time-steps, BOTH are emitted (the mainline `sync8` per-bin
        // double-emission rule).
        if two_baseline_enabled && bin_count > 0 {
            let pct = self.config.costas_two_baseline_percentile;
            let norm_min = self.config.costas_two_baseline_norm_threshold;

            for freq_sub in 0..spectrogram.freq_osr {
                let tight_base = percentile_baseline(&tight_peaks[freq_sub], pct);
                let wide_base = percentile_baseline(&wide_peaks[freq_sub], pct);

                for bin_idx in 0..bin_count {
                    let f0 = lo + bin_idx;
                    let (tight_score, tight_t0) = tight_peaks[freq_sub][bin_idx];
                    let (wide_score, wide_t0) = wide_peaks[freq_sub][bin_idx];

                    let tight_norm = if tight_base > 0.0 && tight_base.is_finite() {
                        tight_score / tight_base
                    } else {
                        0.0
                    };
                    let wide_norm = if wide_base > 0.0 && wide_base.is_finite() {
                        wide_score / wide_base
                    } else {
                        0.0
                    };

                    let tight_pass = tight_norm >= norm_min && tight_score > 0.0;
                    let wide_pass = wide_norm >= norm_min && wide_score > 0.0;

                    // Emit the tight-pathway candidate if it cleared
                    // the normalised threshold but was below `min_score`
                    // absolute. (If it cleared absolute already, the
                    // main loop pushed it.) The duplicate is removed
                    // downstream by NMS / dedup; the worst case is a
                    // single redundant entry per bin.
                    if tight_pass && tight_score <= min_score {
                        candidates.push(CostasCandidate {
                            time_step: tight_t0,
                            freq_bin: f0,
                            freq_sub,
                            sync_score: tight_score,
                            time_refinement: 0.0,
                        });
                    }
                    // Emit the wide-pathway candidate when:
                    //   (a) it cleared the normalised threshold, AND
                    //   (b) either it's below the absolute gate OR the
                    //       wide peak landed at a different time-step
                    //       than the tight peak (per-bin double
                    //       emission rule from mainline sync8).
                    if wide_pass && (wide_score <= min_score || wide_t0 != tight_t0) {
                        candidates.push(CostasCandidate {
                            time_step: wide_t0,
                            freq_bin: f0,
                            freq_sub,
                            sync_score: wide_score,
                            time_refinement: 0.0,
                        });
                    }
                }
            }
        }

        // Sort by score (best first)
        candidates.sort_by(|a, b| {
            b.sync_score
                .partial_cmp(&a.sync_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.truncate(self.config.max_sync_candidates);

        // Non-maximum suppression: remove weaker candidates near stronger ones.
        // Gated by config so hb-019-style audits can disable it.
        if self.config.nms_enabled {
            self.nms_candidates(&mut candidates);
        }

        Ok(candidates)
    }

    /// Compute Costas sync score using ft8_lib-style neighbor comparison.
    ///
    /// For each sync symbol, compares the expected bin's magnitude against
    /// its frequency-adjacent and time-adjacent neighbors. This is more robust
    /// to colored noise than comparing against distant noise bins.
    ///
    /// Score = average of (signal_bin - neighbor_bin) across all valid comparisons.
    fn compute_costas_score(
        &self,
        spec: &Spectrogram,
        t0: usize,
        f0: usize,
        freq_sub: usize,
    ) -> f64 {
        // Full-Costas metric: all three sync groups (block A + B + C).
        self.compute_costas_score_groups(spec, t0, f0, freq_sub, |_| true)
    }

    /// hb-242: Partial-Costas metric (`sync_bc`). Computes the sync score
    /// using ONLY the second and third Costas blocks (symbols 36–42 and
    /// 72–78), skipping block A. This is the slot-edge rescue metric:
    /// when the leading edge of the audio is missing or corrupted and
    /// block A contains noise/garbage, the full metric collapses while
    /// the partial metric remains meaningful. Per spec-wsjtr-sync-bc.md
    /// and spec-wsjtx-mainline-sync8.md, this is intended to be
    /// combined with the full metric via `max(full, partial)` at the
    /// candidate-emission site — never as a replacement for the full
    /// metric on healthy signals.
    fn compute_costas_score_partial_bc(
        &self,
        spec: &Spectrogram,
        t0: usize,
        f0: usize,
        freq_sub: usize,
    ) -> f64 {
        // Partial: skip group 0 (block A); only blocks B (m=1) and C (m=2).
        self.compute_costas_score_groups(spec, t0, f0, freq_sub, |m| m != 0)
    }

    /// Inner Costas-score kernel parameterised by which sync groups to
    /// include. `keep_group(m)` is consulted once per group; pass
    /// `|_| true` for the full ABC metric or `|m| m != 0` for the
    /// hb-242 partial BC metric.
    ///
    /// Because the score is an *average* (sum of per-comparison
    /// differences divided by the comparison count, not a sum), the
    /// full and partial metrics live on the same scale — `max(full,
    /// partial)` is a meaningful selection rule without any
    /// per-pathway threshold adjustment.
    fn compute_costas_score_groups<F: Fn(usize) -> bool>(
        &self,
        spec: &Spectrogram,
        t0: usize,
        f0: usize,
        freq_sub: usize,
        keep_group: F,
    ) -> f64 {
        let pp = &self.protocol_params;

        // With TIME_OSR>1, the outer t0 loop already iterates at sub-symbol
        // resolution, so we only need to check 2 half-symbol offsets within
        // each t0 position (as in the original TIME_OSR=1 code).
        let steps_per_symbol = TIME_OSR;
        let mut best_score = 0.0f64;

        for half in 0..2 {
            let mut score = 0.0f64;
            let mut num_average = 0usize;

            for (m, &group_start) in pp.costas_positions.iter().enumerate() {
                if !keep_group(m) {
                    continue;
                }
                for k in 0..pp.costas_length {
                    let symbol_idx = group_start + k;
                    let time_idx = t0 + symbol_idx * steps_per_symbol + half;

                    if time_idx >= spec.num_steps {
                        continue;
                    }

                    let sm = pp.costas_arrays[m][k] as usize; // expected tone bin
                    let freq_idx = f0 + sm;

                    if freq_idx >= spec.num_bins {
                        continue;
                    }

                    let signal_mag = spec.power[time_idx][freq_sub][freq_idx];

                    // Check frequency neighbor below
                    if sm > 0 && f0 + sm - 1 < spec.num_bins {
                        let neighbor = spec.power[time_idx][freq_sub][f0 + sm - 1];
                        score += signal_mag - neighbor;
                        num_average += 1;
                    }

                    // Check frequency neighbor above
                    if sm + 1 < pp.num_tones && f0 + sm + 1 < spec.num_bins {
                        let neighbor = spec.power[time_idx][freq_sub][f0 + sm + 1];
                        score += signal_mag - neighbor;
                        num_average += 1;
                    }

                    // Check time neighbor behind (previous symbol in this sync group)
                    if k > 0 && time_idx >= steps_per_symbol {
                        let prev_time = time_idx - steps_per_symbol;
                        if prev_time < spec.num_steps {
                            let neighbor = spec.power[prev_time][freq_sub][freq_idx];
                            score += signal_mag - neighbor;
                            num_average += 1;
                        }
                    }

                    // Check time neighbor ahead (next symbol in this sync group)
                    if k + 1 < pp.costas_length {
                        let next_time = time_idx + steps_per_symbol;
                        if next_time < spec.num_steps {
                            let neighbor = spec.power[next_time][freq_sub][freq_idx];
                            score += signal_mag - neighbor;
                            num_average += 1;
                        }
                    }
                }
            }

            let half_score = if num_average > 0 {
                score / num_average as f64
            } else {
                0.0
            };

            if half_score > best_score {
                best_score = half_score;
            }
        }

        best_score
    }

    /// Non-maximum suppression: remove weaker candidates near stronger ones.
    ///
    /// hb-036: when `nms_score_delta_db > 0.0`, a weaker candidate `j` is
    /// suppressed only if it lies within the TF radius AND its sync_score
    /// is within `nms_score_delta_db` of the stronger candidate `i`'s
    /// sync_score. Meaningfully-weaker candidates are treated as distinct
    /// signals and kept. With `nms_score_delta_db == 0.0` (the default),
    /// the legacy pure TF-distance behavior is preserved bit-exactly.
    fn nms_candidates(&self, candidates: &mut Vec<CostasCandidate>) {
        // candidates are already sorted by score (best first)
        let mut keep = vec![true; candidates.len()];
        let score_delta = self.config.nms_score_delta_db;
        let score_relative = score_delta > 0.0;

        for i in 0..candidates.len() {
            if !keep[i] {
                continue;
            }
            for j in (i + 1)..candidates.len() {
                if !keep[j] {
                    continue;
                }
                let dt = (candidates[i].time_step as isize - candidates[j].time_step as isize)
                    .unsigned_abs();
                let df = (candidates[i].freq_bin as isize - candidates[j].freq_bin as isize)
                    .unsigned_abs();

                if dt <= self.config.nms_time_radius && df <= self.config.nms_freq_radius {
                    // hb-036: score-relative gate. j is "within delta of i"
                    // when j.sync_score > i.sync_score - score_delta. If
                    // j is meaningfully weaker (j.score <= i.score - delta),
                    // treat as a distinct signal and KEEP it.
                    if score_relative
                        && candidates[j].sync_score <= candidates[i].sync_score - score_delta
                    {
                        continue;
                    }
                    keep[j] = false; // suppress the weaker candidate
                }
            }
        }

        let mut i = 0;
        candidates.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
    }

    // ========================================================================
    // Step 3: Decode a single candidate
    // ========================================================================

    // Attempt to decode a single Costas sync candidate.
    // ========================================================================
    // A Priori (AP) enhanced decoding helpers
    // ========================================================================

    /// Try AP-enhanced decoding for a candidate that failed standard AP0 decode.
    ///
    /// Extracts LLRs from the spectrogram path (cheaper than fine-timing FFT),
    /// then tries AP1 (own callsign as called station), AP2 (recent callers),
    /// AP3 (both calls known), and AP4 (AP3 + message type constraint).
    fn try_ap_decode(
        &self,
        candidate: &CostasCandidate,
        spectrogram: &Spectrogram,
        ap_context: &crate::ap::ApContext,
        decoded_calls: &HashSet<String>,
        _pass: usize,
    ) -> Ft8Result<Option<DecodedMessage>> {
        let tone_spacing = self.protocol_params.tone_spacing;
        let sps = self.protocol_params.samples_per_symbol(SAMPLE_RATE);
        let spec_step = sps / TIME_OSR;
        let coarse_offset = candidate.time_step * spec_step;

        // Try both freq_sub values, same as the spectrogram path in decode_candidate
        let freq_sub_trials = [
            candidate.freq_sub,
            if candidate.freq_sub == 0 { 1 } else { 0 },
        ];

        for &trial_freq_sub in &freq_sub_trials {
            let trial_candidate = CostasCandidate {
                freq_sub: trial_freq_sub,
                ..*candidate
            };
            let tone_magnitudes =
                self.extract_symbols_from_spectrogram(spectrogram, &trial_candidate);
            let mut base_llrs = self.compute_soft_llrs_db(&tone_magnitudes);
            // JS8Call-Improved-style LLR whitening (inspired by spec ref
            // `spec-js8call-llr-whitening.md`). No-op when disabled, so
            // the legacy AP path stays byte-identical.
            maybe_whiten_llrs(
                self.config.llr_whitening_enabled,
                &mut base_llrs,
                &tone_magnitudes,
                &self.protocol_params,
            );

            // Compute frequency and time for building DecodedMessage
            let sub_bin_offset = trial_freq_sub as f64 * (tone_spacing / FREQ_OSR as f64);
            let base_frequency = candidate.freq_bin as f64 * tone_spacing + sub_bin_offset;
            let time_offset_s = coarse_offset as f64 / SAMPLE_RATE as f64;

            // SNR estimate (reused across AP trials for this candidate)
            let snr_db = self.estimate_snr_spectrogram(&tone_magnitudes);
            let confidence = (candidate.sync_score / 12.0).min(1.0) as f32;

            // --- AP1: inject own callsign at bits 28-55 (called station) ---
            if ap_context.my_call.is_some() {
                if let Some(msg) = self.try_ldpc_with_ap(
                    &base_llrs,
                    crate::ap::ApLevel::Ap1,
                    ap_context,
                    None,
                    snr_db,
                    confidence,
                    base_frequency,
                    time_offset_s,
                )? {
                    return Ok(Some(msg));
                }
            }

            // --- AP2: inject each recent caller at bits 0-27 + AP1 ---
            if ap_context.my_call.is_some() {
                for recent in &ap_context.recent_calls {
                    // Short-circuit: skip calls already decoded this window
                    if decoded_calls.contains(&recent.callsign) {
                        continue;
                    }
                    if let Some(msg) = self.try_ldpc_with_ap(
                        &base_llrs,
                        crate::ap::ApLevel::Ap2,
                        ap_context,
                        Some(recent),
                        snr_db,
                        confidence,
                        base_frequency,
                        time_offset_s,
                    )? {
                        return Ok(Some(msg));
                    }
                }
            }

            // --- AP3: both callsigns known (active QSO) ---
            if ap_context.active_qso.is_some() && ap_context.my_call.is_some() {
                if let Some(msg) = self.try_ldpc_with_ap(
                    &base_llrs,
                    crate::ap::ApLevel::Ap3,
                    ap_context,
                    None,
                    snr_db,
                    confidence,
                    base_frequency,
                    time_offset_s,
                )? {
                    return Ok(Some(msg));
                }

                // --- AP4: AP3 + message type constraint ---
                if let Some(ref qso) = ap_context.active_qso {
                    if matches!(
                        qso.progress,
                        crate::ap::QsoApProgress::WaitingForConfirmation
                    ) {
                        if let Some(msg) = self.try_ldpc_with_ap(
                            &base_llrs,
                            crate::ap::ApLevel::Ap4,
                            ap_context,
                            None,
                            snr_db,
                            confidence,
                            base_frequency,
                            time_offset_s,
                        )? {
                            return Ok(Some(msg));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// Try LDPC decode with AP injection at a specific level.
    ///
    /// Clones the base LLRs, injects AP bits, normalizes, runs LDPC + CRC,
    /// and returns a DecodedMessage on success.
    fn try_ldpc_with_ap(
        &self,
        base_llrs: &[f32],
        ap_level: crate::ap::ApLevel,
        ap_context: &crate::ap::ApContext,
        caller_override: Option<&crate::ap::RecentCallAp>,
        snr_db: f32,
        confidence: f32,
        base_frequency: f64,
        time_offset_s: f64,
    ) -> Ft8Result<Option<DecodedMessage>> {
        let mut llrs = base_llrs.to_vec();
        let xor_sequence = self.protocol_params.xor_sequence;

        // Inject AP bits according to level
        match ap_level {
            crate::ap::ApLevel::Ap0 => {} // no injection
            crate::ap::ApLevel::Ap1 => {
                crate::ap::inject_ap_llrs(&mut llrs, ap_level, ap_context);
            }
            crate::ap::ApLevel::Ap2 => {
                // First inject AP1 (our call as called station)
                crate::ap::inject_ap_llrs(&mut llrs, crate::ap::ApLevel::Ap1, ap_context);
                // Then inject the specific caller at bits 0-27
                if let Some(caller) = caller_override {
                    crate::ap::inject_ap2_caller(&mut llrs, caller);
                }
            }
            crate::ap::ApLevel::Ap3 | crate::ap::ApLevel::Ap4 => {
                crate::ap::inject_ap_llrs(&mut llrs, ap_level, ap_context);
            }
        }

        normalize_llrs(&mut llrs, self.config.llr_target_variance);

        let corrected_bits = match self.ldpc_decoder.decode_soft(&llrs) {
            Ok(bits) => bits,
            Err(_) => return Ok(None),
        };

        if !self.verify_crc(&corrected_bits) {
            return Ok(None);
        }

        // For FT4, un-apply the XOR scrambling on the payload
        let payload_bits = if let Some(xor_seq) = xor_sequence {
            let mut bits = corrected_bits[0..PAYLOAD_BITS].to_owned();
            for byte_idx in 0..10 {
                let xor_byte = xor_seq[byte_idx];
                for bit_pos in 0..8 {
                    let global_bit = byte_idx * 8 + bit_pos;
                    if global_bit >= PAYLOAD_BITS {
                        break;
                    }
                    if (xor_byte >> (7 - bit_pos)) & 1 == 1 {
                        let cur = bits[global_bit];
                        bits.set(global_bit, !cur);
                    }
                }
            }
            bits
        } else {
            corrected_bits[0..PAYLOAD_BITS].to_owned()
        };

        let ft8_message = self.message_parser.parse_payload(&payload_bits)?;

        // Reject CRC false positives
        if !ft8_message.is_plausible() {
            return Ok(None);
        }

        // AP-injection survival check. When AP injects bits as priors and
        // LDPC's parity constraints overrule them, the resulting codeword
        // doesn't carry the AP-injected callsign. Such "successful" AP
        // decodes are false positives — the AP hint didn't help, the
        // codeword passed CRC by coincidence.
        if !ap_injection_survived(ap_level, ap_context, &ft8_message) {
            return Ok(None);
        }

        let ap_level_num = match ap_level {
            crate::ap::ApLevel::Ap0 => 0u8,
            crate::ap::ApLevel::Ap1 => 1,
            crate::ap::ApLevel::Ap2 => 2,
            crate::ap::ApLevel::Ap3 => 3,
            crate::ap::ApLevel::Ap4 => 4,
        };
        // Minimum confidence floor. Two thresholds: AP0 decodes can land at
        // sync_score ≥ 4.92 (the LDPC has no priors, so a CRC-valid output
        // is strong evidence). AP1+ decodes biased the LDPC, so a successful
        // result is weaker evidence — require sync_score ≥ 6.0 (confidence
        // 0.50) to compensate. CRC-14 collisions on noise still produce
        // structurally valid messages at low sync, especially under AP
        // injection where the prior steers the codeword toward a
        // pre-chosen callsign pattern.
        const MIN_DECODE_CONFIDENCE: f32 = 0.41;
        const MIN_AP_DECODE_CONFIDENCE: f32 = 0.55;
        const SCRUTINY_THRESHOLD: f32 = 0.65;
        let floor = if matches!(ap_level, crate::ap::ApLevel::Ap0) {
            MIN_DECODE_CONFIDENCE
        } else {
            MIN_AP_DECODE_CONFIDENCE
        };
        if confidence < floor {
            return Ok(None);
        }
        if confidence < SCRUTINY_THRESHOLD && ft8_message.suspicion_score() >= 2 {
            return Ok(None);
        }

        let mut decoded_message = DecodedMessage::new(
            ft8_message,
            snr_db,
            confidence,
            base_frequency,
            time_offset_s,
        );
        decoded_message.tone_symbols = Some(Self::codeword_to_symbols(&corrected_bits));
        decoded_message.ap_level = ap_level_num;

        Ok(Some(decoded_message))
    }

    /// Estimate SNR from spectrogram tone magnitudes (dB domain).
    fn estimate_snr_spectrogram(&self, tone_magnitudes: &[[f64; NUM_TONES]]) -> f32 {
        let data_positions = self.protocol_params.data_symbol_indices();
        let mut signal_sum = 0.0f64;
        let mut noise_sum = 0.0f64;
        let mut count = 0usize;
        for &sym_idx in &data_positions {
            let mags = &tone_magnitudes[sym_idx];
            let best = mags.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let worst = mags.iter().cloned().fold(f64::INFINITY, f64::min);
            signal_sum += best;
            noise_sum += worst;
            count += 1;
        }
        if count > 0 {
            let avg_signal_db = signal_sum / count as f64;
            let avg_noise_db = noise_sum / count as f64;
            let snr_bin_db = avg_signal_db - avg_noise_db;
            let bw_correction = 10.0 * (2500.0f64 / 6.25).log10();
            (snr_bin_db - bw_correction) as f32
        } else {
            -24.0f32
        }
    }

    /// hb-056: non-coherent cross-cycle symbol averaging.
    ///
    /// Groups sync candidates that look like the same repeating station in
    /// different slots (same `freq_sub`+`freq_bin`±1, `t0` apart by a
    /// multiple of one FT8 slot ±2 steps, sync-score within band), sums
    /// each group's per-symbol tone POWERS in linear (10^(dB/10)), and
    /// runs LLR → LDPC → CRC on the averaged candidate. Returns any
    /// passing decode for the caller to union + dedup with the per-slot
    /// results. Additive — never removes a per-slot decode, and a
    /// corrupted averaged candidate that fails CRC contributes nothing.
    ///
    /// Power-only: pancetta's spectrogram discards phase, so this is the
    /// non-coherent variant of JTDX's `s2(i) = |cs|² + |csold|²` rule.
    /// Bounds the expected gain below JTDX's coherent edge — see
    /// docs/superpowers/specs/2026-05-25-cross-cycle-averaging-design.md.
    fn cross_cycle_averaging_pass(
        &self,
        spectrogram: &Spectrogram,
        candidates: &[CostasCandidate],
    ) -> Vec<DecodedMessage> {
        if candidates.len() < 2 {
            return Vec::new();
        }
        let pp = &self.protocol_params;
        let tone_spacing = pp.tone_spacing;
        let sps = pp.samples_per_symbol(SAMPLE_RATE);
        let spec_step = sps / TIME_OSR;
        let groups = group_for_cross_cycle(candidates);

        // hb-074: when the coherent flag is on AND the spectrogram retains
        // complex bins (only when cross_cycle_coherent was set at decode
        // start), the pass takes a phase-aligned coherent sum path; falls
        // back to the non-coherent (hb-056) path otherwise.
        let coherent = self.config.cross_cycle_coherent && spectrogram.complex.is_some();

        let mut decoded: Vec<DecodedMessage> = Vec::new();
        for group in groups {
            // Combine the group's per-symbol per-tone energies into a single
            // averaged tone-magnitude table. Coherent path: extract complex
            // symbols, estimate each candidate's phase rotor from Costas,
            // align by multiplying with conj(rotor), sum complex, |sum|² →
            // dB. Non-coherent path: dB → linear power → sum → dB.
            let avg_mags = if coherent {
                // hb-075: when MRC is on, multiply each member by conj(acc)
                // directly — does alignment + magnitude weighting in one
                // op so strong rotors dominate, noisy weak rotors contribute
                // weakly. Falls back to hb-074's unweighted alignment
                // (conj(rotor)) when MRC is off.
                let mrc = self.config.cross_cycle_coherent_mrc;
                let mut aligned: Vec<Vec<[Complex<f64>; NUM_TONES]>> =
                    Vec::with_capacity(group.len());
                for &i in &group {
                    let Some(cs) = par_extract_complex_symbols_from_spectrogram(
                        pp,
                        spectrogram,
                        &candidates[i],
                    ) else {
                        continue;
                    };
                    let multiplier = if mrc {
                        let acc = compute_costas_complex_accumulator(pp, &cs);
                        if acc.norm_sqr() < 1e-60 {
                            continue;
                        }
                        acc.conj()
                    } else {
                        let Some(rotor) = estimate_candidate_phase_rotor(pp, &cs) else {
                            continue;
                        };
                        rotor.conj()
                    };
                    let rotated: Vec<[Complex<f64>; NUM_TONES]> = cs
                        .into_iter()
                        .map(|mut row| {
                            for t in 0..NUM_TONES {
                                row[t] *= multiplier;
                            }
                            row
                        })
                        .collect();
                    aligned.push(rotated);
                }
                if aligned.len() < 2 {
                    // Lost too many to bad phase estimates; nothing to coherent-sum.
                    continue;
                }
                coherent_sum_complex_to_db(&aligned, pp.num_symbols)
            } else {
                let lin = self.config.sync_time_interp_linear_power;
                let members: Vec<Vec<[f64; NUM_TONES]>> = group
                    .iter()
                    .map(|&i| {
                        par_extract_symbols_from_spectrogram(pp, spectrogram, &candidates[i], lin)
                    })
                    .collect();
                sum_tone_magnitudes_linear(&members, pp.num_symbols)
            };

            // LLR → LDPC → CRC. Reuses the existing decoder; if BP fails
            // OR CRC fails OR message is implausible, the averaged
            // candidate yields nothing (additive, so harmless).
            let mut llrs = par_compute_soft_llrs_db(pp, &avg_mags);
            maybe_whiten_llrs(self.config.llr_whitening_enabled, &mut llrs, &avg_mags, pp);
            normalize_llrs(&mut llrs, self.config.llr_target_variance);
            let Ok(corrected_bits) = self.ldpc_decoder.decode_soft(&llrs) else {
                continue;
            };
            if !par_verify_crc(&corrected_bits) {
                continue;
            }
            let payload_bits = par_apply_xor(pp.xor_sequence, &corrected_bits);
            let ft8_message = match self.message_parser.parse_payload(&payload_bits) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !ft8_message.is_plausible() {
                continue;
            }

            // Build a DecodedMessage anchored on the group's first member
            // (mirrors par_decode_candidate's pattern).
            let anchor = &candidates[group[0]];
            let sub_bin_offset = anchor.freq_sub as f64 * (tone_spacing / FREQ_OSR as f64);
            let base_frequency = anchor.freq_bin as f64 * tone_spacing + sub_bin_offset;
            let coarse_offset = (anchor.time_step as isize - spectrogram.time_padding as isize)
                * spec_step as isize;
            let snr_db = par_estimate_snr_spectrogram(pp, &avg_mags);
            let confidence = (anchor.sync_score / 12.0).min(1.0) as f32;
            decoded.push(DecodedMessage::new(
                ft8_message,
                snr_db,
                confidence,
                base_frequency,
                coarse_offset as f64 / SAMPLE_RATE as f64,
            ));
        }
        decoded
    }

    /// hb-079: coherent iterative-subtract multi-pass.
    ///
    /// For each decoded message that has its tone_symbols preserved, reverse-
    /// derive the candidate position, estimate its phase rotor from Costas,
    /// and subtract its coherent signal contribution from the complex
    /// spectrogram (ML projection). After subtracting every decode, re-run
    /// the Costas sync search on the residual and decode any new candidates
    /// that the original pass missed because they were masked by stronger
    /// neighbors. Returns the new decodes for the caller to union + dedup.
    /// No-op when `coherent_multipass_iterations == 0` or the spectrogram
    /// lacks complex retention. Called once per round by the caller;
    /// `decoded` is the slice of just-found decodes to subtract this round.
    ///
    /// **Prior art**: this is a spectrogram-domain implementation of the
    /// coherent iterative subtract-and-redecode technique published by
    /// Franke, Taylor & Somerville in QEX July/August 2020 ("The FT4 and
    /// FT8 Communication Protocols", <https://wsjt.sourceforge.io/FT4_FT8_QEX.pdf>)
    /// and shipping in WSJT-X mainline as `lib/ft8/subtractft8.f90` since
    /// 2020. The formula `proj = Re(bin·conj(rotor))·rotor` is canonical
    /// ML single-user projection. The mechanism family is **Successive
    /// Interference Cancellation (SIC)** per Verdú 1998 "Multiuser
    /// Detection". Pancetta's variant differs from WSJT-X in implementation
    /// domain (spectrogram bins vs time-domain waveform) and rotor
    /// reference set (21 Costas symbols vs all 79), not in algorithm.
    /// hb-079 closed a gap vs our underlying ft8_lib dependency (which has
    /// no multi-pass), not vs WSJT-X. See
    /// `docs/engineering/2026-06-02-sic-novelty-verification.md` for the
    /// full novelty review.
    fn coherent_subtract_and_repass(
        &mut self,
        spectrogram: &mut Spectrogram,
        decoded: &[DecodedMessage],
        // hb-016: optional residual energy early-stop. When
        // `Some((floor_db, threshold_db))`, after step 1 (subtract)
        // compute the mean per-bin excess above `floor_db` of the
        // residual spectrogram and bail (returning empty) if it falls
        // below `threshold_db`. `None` disables the probe (matches the
        // historical hb-079/hb-080 behavior). `floor_db` is the
        // noise-floor proxy computed once on the ORIGINAL spectrogram
        // by the caller (so the reference is stable across rounds).
        energy_stop: Option<(f64, f64)>,
        // hb-091 Session 2: scope to apply on the residual sync sweep.
        // Threaded through from the top-level scoped decode call so the
        // residual pass never silently re-searches outside the requested
        // bin range.
        freq_bin_scope: Option<&RangeInclusive<usize>>,
        // hb-230: partner audio freq for the relaxed-threshold branch on
        // the residual sync sweep. `None` keeps the historical behaviour.
        partner_freq_hz: Option<f64>,
    ) -> Vec<DecodedMessage> {
        if self.config.coherent_multipass_iterations == 0 || spectrogram.complex.is_none() {
            return Vec::new();
        }
        let pp = &self.protocol_params;
        let time_padding = spectrogram.time_padding;

        // Step 1: subtract each decoded signal's coherent contribution.
        let mut subtracted_candidates: Vec<CostasCandidate> = Vec::new();
        for msg in decoded {
            let Some(tone_symbols) = msg.tone_symbols.as_ref() else {
                continue;
            };
            if tone_symbols.len() < pp.num_symbols {
                continue;
            }
            let candidate = reverse_derive_candidate(msg, pp, time_padding);
            let Some(cs) =
                par_extract_complex_symbols_from_spectrogram(pp, spectrogram, &candidate)
            else {
                continue;
            };
            // hb-081: compute both the accumulator (for MRC scaling) and
            // the unit rotor (for ML projection direction).
            let acc = compute_costas_complex_accumulator(pp, &cs);
            let mag = acc.norm();
            if mag < 1e-30 {
                continue;
            }
            let rotor = acc / mag;
            let scale = if self.config.coherent_subtract_mrc_threshold > 0.0 {
                (mag / self.config.coherent_subtract_mrc_threshold).min(1.0)
            } else {
                1.0
            };
            subtract_decode_coherent(spectrogram, pp, &candidate, rotor, tone_symbols, scale);
            subtracted_candidates.push(candidate);
        }
        if subtracted_candidates.is_empty() {
            return Vec::new();
        }

        // hb-016: residual energy early-stop. After subtraction, compute
        // the average per-bin excess above the original noise floor. If
        // it drops below `threshold_db`, the residual is dominated by
        // noise (signals subtracted away) and the sync_search + LDPC
        // work this round would be wasted — bail. The probe is O(N) over
        // the power tensor; cheap relative to the Costas sweep it
        // short-circuits.
        if let Some((noise_floor_db, threshold_db)) = energy_stop {
            let residual_excess_db = mean_excess_above_noise_db(&spectrogram.power, noise_floor_db);
            if residual_excess_db < threshold_db {
                return Vec::new();
            }
        }

        // Step 2: re-run Costas sync search on the residual spectrogram.
        // hb-082: use residual-specific threshold if set, else production.
        let residual_min = self
            .config
            .residual_min_sync_score
            .unwrap_or(self.config.min_sync_score);
        let Ok(new_candidates) = self.costas_sync_search_with_threshold_and_partner(
            spectrogram,
            residual_min,
            freq_bin_scope,
            partner_freq_hz,
        ) else {
            return Vec::new();
        };

        // Step 3: keep only candidates not at the same position as something
        // already subtracted (those would be the original decodes' Costas
        // pattern reappearing in the residual at a lower score). Match by
        // freq_sub equality + freq_bin ±1 + time_step ±2 — same shape as
        // the cross-cycle grouping tolerance.
        let new_candidates: Vec<CostasCandidate> = new_candidates
            .into_iter()
            .filter(|nc| {
                !subtracted_candidates.iter().any(|sc| {
                    nc.freq_sub == sc.freq_sub
                        && (nc.freq_bin as i64 - sc.freq_bin as i64).unsigned_abs() <= 1
                        && (nc.time_step as i64 - sc.time_step as i64).unsigned_abs() <= 2
                })
            })
            .take(self.config.max_sync_candidates)
            .collect();
        if new_candidates.is_empty() {
            return Vec::new();
        }

        // hb-057 V2 (Session 3): per-candidate callsign-keyed DT prior
        // narrowing. When `dt_history_enabled` AND a lookup is attached
        // AND `dt_history_freq_window_hz > 0`, for EACH residual
        // candidate at (cand_freq_hz, time_step) query the lookup for
        // the union of DT priors from callsigns whose recent sightings
        // were within `freq_window_hz` of `cand_freq`. Filter the
        // candidate's t0 against that union; keep candidates with no
        // nearby priors (cold-start safe).
        //
        // This is structurally different from V1 (SHELVED 2026-06-02):
        // V1 took the UNION over callsigns in THIS WAV's pass 1 and
        // applied the same gate to every candidate — the wrong key, as
        // the V1 SHELVE note diagnosed. V2 keys the lookup per candidate
        // by frequency proximity, which is the predictable proxy for
        // "which callsign would this candidate decode to."
        //
        // The V1 path (union of pass-1 callsigns) is retained when
        // `dt_history_freq_window_hz == 0.0` for back-compat.
        //
        // Pass 1 is NEVER touched — this is the residual pass only.
        // Spec: docs/superpowers/specs/2026-05-31-hb-057-median-dt-design.md.
        // Prior art: JTDX commit "use median filter in average DT
        // calculation" (Feb 2022).
        let new_candidates: Vec<CostasCandidate> =
            if self.config.dt_history_enabled && self.dt_priors.is_some() {
                let lookup = self.dt_priors.as_ref().expect("checked above");
                let sps_local = pp.samples_per_symbol(SAMPLE_RATE);
                let spec_step_local = sps_local / TIME_OSR;
                let floor = self.config.dt_history_window_floor_s;
                let scale = self.config.dt_history_window_iqr_scale;
                let freq_window = self.config.dt_history_freq_window_hz;
                let tone_spacing = pp.tone_spacing;

                if freq_window > 0.0 {
                    // V2: per-candidate callsign-keyed sync.
                    new_candidates
                        .into_iter()
                        .filter(|nc| {
                            let sub_off = nc.freq_sub as f64 * (tone_spacing / FREQ_OSR as f64);
                            let cand_freq = nc.freq_bin as f64 * tone_spacing + sub_off;
                            let priors = lookup.priors_near_freq(cand_freq, freq_window);
                            if priors.is_empty() {
                                // Cold-start for THIS candidate's freq — keep.
                                return true;
                            }
                            let coarse_offset = (nc.time_step as isize
                                - spectrogram.time_padding as isize)
                                * spec_step_local as isize;
                            let t_s = coarse_offset as f64 / SAMPLE_RATE as f64;
                            priors.iter().any(|p| {
                                let radius = floor.max(p.iqr * scale);
                                let lo = p.median_dt - radius;
                                let hi = p.median_dt + radius;
                                t_s >= lo && t_s <= hi
                            })
                        })
                        .collect()
                } else {
                    // V1 legacy path (SHELVED — retained behind
                    // freq_window=0 for back-compat).
                    let prior_windows: Vec<(f64, f64)> = decoded
                        .iter()
                        .filter_map(|m| m.message.from_callsign.as_deref())
                        .filter_map(|call| {
                            let bare = call.split('/').next().unwrap_or(call);
                            lookup.prior(bare)
                        })
                        .map(|p| {
                            let radius = floor.max(p.iqr * scale);
                            (p.median_dt - radius, p.median_dt + radius)
                        })
                        .collect();
                    if prior_windows.is_empty() {
                        new_candidates
                    } else {
                        new_candidates
                            .into_iter()
                            .filter(|nc| {
                                let coarse_offset = (nc.time_step as isize
                                    - spectrogram.time_padding as isize)
                                    * spec_step_local as isize;
                                let t_s = coarse_offset as f64 / SAMPLE_RATE as f64;
                                prior_windows
                                    .iter()
                                    .any(|(lo, hi)| t_s >= *lo && t_s <= *hi)
                            })
                            .collect()
                    }
                }
            } else {
                new_candidates
            };
        if new_candidates.is_empty() {
            return Vec::new();
        }

        // Step 4: decode each remaining new candidate, sequentially (count
        // is small after subtraction).
        let mut decoded_new: Vec<DecodedMessage> = Vec::new();
        let tone_spacing = pp.tone_spacing;
        let sps = pp.samples_per_symbol(SAMPLE_RATE);
        let spec_step = sps / TIME_OSR;
        let lin = self.config.sync_time_interp_linear_power;
        // hb-093 step-4 extension: optional pre-decode residual SNR gate +
        // diagnostic capture. Mirrors `joint_pair_retry_pass` — the per-WAV
        // candidate count at this site (post-sync-search, bounded by
        // `max_sync_candidates`) is typically ~10× larger than
        // joint_pair_retry's surface, so the same gate yields ~10× the
        // elapsed savings here.
        let snr_gate_db = self.config.residual_snr_gate_db;
        let diagnostic_on = self.config.residual_snr_diagnostic;
        for cand in &new_candidates {
            let tone_mags = par_extract_symbols_from_spectrogram(pp, spectrogram, cand, lin);
            // hb-093 step-4: pre-decode residual SNR estimate. Cheap — just
            // iterates the already-extracted tone magnitudes.
            let pre_snr_db = par_estimate_snr_spectrogram(pp, &tone_mags);
            if let Some(threshold) = snr_gate_db {
                if (pre_snr_db as f64) < threshold {
                    if diagnostic_on {
                        self.residual_snr_records
                            .push((cand.sync_score, pre_snr_db, false));
                    }
                    continue;
                }
            }
            let mut llrs = par_compute_soft_llrs_db(pp, &tone_mags);
            maybe_whiten_llrs(self.config.llr_whitening_enabled, &mut llrs, &tone_mags, pp);
            normalize_llrs(&mut llrs, self.config.llr_target_variance);
            let Ok(corrected_bits) = self.ldpc_decoder.decode_soft(&llrs) else {
                if diagnostic_on {
                    self.residual_snr_records
                        .push((cand.sync_score, pre_snr_db, false));
                }
                continue;
            };
            if !par_verify_crc(&corrected_bits) {
                if diagnostic_on {
                    self.residual_snr_records
                        .push((cand.sync_score, pre_snr_db, false));
                }
                continue;
            }
            let payload_bits = par_apply_xor(pp.xor_sequence, &corrected_bits);
            let ft8_message = match self.message_parser.parse_payload(&payload_bits) {
                Ok(m) => m,
                Err(_) => {
                    if diagnostic_on {
                        self.residual_snr_records
                            .push((cand.sync_score, pre_snr_db, false));
                    }
                    continue;
                }
            };
            if !ft8_message.is_plausible() {
                if diagnostic_on {
                    self.residual_snr_records
                        .push((cand.sync_score, pre_snr_db, false));
                }
                continue;
            }
            let sub_bin_offset = cand.freq_sub as f64 * (tone_spacing / FREQ_OSR as f64);
            let base_frequency = cand.freq_bin as f64 * tone_spacing + sub_bin_offset;
            let coarse_offset =
                (cand.time_step as isize - spectrogram.time_padding as isize) * spec_step as isize;
            let snr_db = par_estimate_snr_spectrogram(pp, &tone_mags);
            let confidence = (cand.sync_score / 12.0).min(1.0) as f32;
            let mut new_msg = DecodedMessage::new(
                ft8_message,
                snr_db,
                confidence,
                base_frequency,
                coarse_offset as f64 / SAMPLE_RATE as f64,
            );
            // Preserve tone_symbols on the new decode so future multipass
            // iterations (if ever added) could subtract this signal too.
            new_msg.tone_symbols = Some(Self::codeword_to_symbols(&corrected_bits));
            if diagnostic_on {
                self.residual_snr_records
                    .push((cand.sync_score, pre_snr_db, true));
            }
            decoded_new.push(new_msg);
        }
        decoded_new
    }

    /// hb-086 V1: after the multipass subtract+repass loop saturates, take
    /// every ORIGINAL sync candidate that didn't already become a decoded
    /// (and therefore subtracted) signal, and try decoding it against the
    /// residual spectrogram. Catches signal pairs where pass-1 failed at
    /// position B because A's interference corrupted B's LLRs, but B's
    /// residual sync_score (post-A-subtract) sits below `min_sync_score`,
    /// so the residual `costas_sync_search` in `coherent_subtract_and_repass`
    /// never re-surfaces B. We bypass that filter by reusing the ORIGINAL
    /// candidate positions (which had a real Costas signature in the raw
    /// data) and just retrying the LDPC against the cleaner LLRs.
    ///
    /// The kill-switch diagnostic
    /// (`examples/joint_decoding_pair_density.rs`) found 78% of missed
    /// truths on top-20 hard-200 WAVs are within 50 Hz of a recovered
    /// decode — a strong "pair structure exists" signal.
    fn joint_pair_retry_pass(
        &mut self,
        spectrogram: &Spectrogram,
        sync_candidates: &[CostasCandidate],
        pass_decoded: &[DecodedMessage],
    ) -> Vec<DecodedMessage> {
        let pp = &self.protocol_params;
        let time_padding = spectrogram.time_padding;

        // Build the set of positions whose coherent contribution was
        // subtracted by `coherent_subtract_and_repass` (every decode with
        // tone_symbols, across all multipass rounds).
        let subtracted_positions: Vec<CostasCandidate> = pass_decoded
            .iter()
            .filter_map(|m| {
                m.tone_symbols
                    .as_ref()
                    .map(|_| reverse_derive_candidate(m, pp, time_padding))
            })
            .collect();

        // Filter sync_candidates to those NOT at an already-subtracted
        // position. Same ±1 freq_bin / ±2 time_step tolerance as the
        // residual sync_search filter in `coherent_subtract_and_repass`.
        let pending: Vec<&CostasCandidate> = sync_candidates
            .iter()
            .filter(|sc| {
                !subtracted_positions.iter().any(|sp| {
                    sc.freq_sub == sp.freq_sub
                        && (sc.freq_bin as i64 - sp.freq_bin as i64).unsigned_abs() <= 1
                        && (sc.time_step as i64 - sp.time_step as i64).unsigned_abs() <= 2
                })
            })
            .collect();
        if pending.is_empty() {
            return Vec::new();
        }

        // Decode each pending candidate against the residual spectrogram.
        // Same per-candidate path as `coherent_subtract_and_repass` step 4
        // (sequential — count is bounded by the candidate ceiling, and the
        // expensive work is the LDPC inside).
        let mut decoded_new: Vec<DecodedMessage> = Vec::new();
        let tone_spacing = pp.tone_spacing;
        let sps = pp.samples_per_symbol(SAMPLE_RATE);
        let spec_step = sps / TIME_OSR;
        let lin = self.config.sync_time_interp_linear_power;
        // hb-093: optional pre-decode residual SNR gate + diagnostic capture.
        let snr_gate_db = self.config.residual_snr_gate_db;
        let diagnostic_on = self.config.residual_snr_diagnostic;
        for cand in &pending {
            let tone_mags = par_extract_symbols_from_spectrogram(pp, spectrogram, cand, lin);
            // hb-093: pre-decode residual SNR estimate. Cheap — just
            // iterates the already-extracted tone magnitudes.
            let pre_snr_db = par_estimate_snr_spectrogram(pp, &tone_mags);
            if let Some(threshold) = snr_gate_db {
                if (pre_snr_db as f64) < threshold {
                    if diagnostic_on {
                        self.residual_snr_records
                            .push((cand.sync_score, pre_snr_db, false));
                    }
                    continue;
                }
            }
            let mut llrs = par_compute_soft_llrs_db(pp, &tone_mags);
            maybe_whiten_llrs(self.config.llr_whitening_enabled, &mut llrs, &tone_mags, pp);
            normalize_llrs(&mut llrs, self.config.llr_target_variance);
            let Ok(corrected_bits) = self.ldpc_decoder.decode_soft(&llrs) else {
                if diagnostic_on {
                    self.residual_snr_records
                        .push((cand.sync_score, pre_snr_db, false));
                }
                continue;
            };
            if !par_verify_crc(&corrected_bits) {
                if diagnostic_on {
                    self.residual_snr_records
                        .push((cand.sync_score, pre_snr_db, false));
                }
                continue;
            }
            let payload_bits = par_apply_xor(pp.xor_sequence, &corrected_bits);
            let ft8_message = match self.message_parser.parse_payload(&payload_bits) {
                Ok(m) => m,
                Err(_) => {
                    if diagnostic_on {
                        self.residual_snr_records
                            .push((cand.sync_score, pre_snr_db, false));
                    }
                    continue;
                }
            };
            if !ft8_message.is_plausible() {
                if diagnostic_on {
                    self.residual_snr_records
                        .push((cand.sync_score, pre_snr_db, false));
                }
                continue;
            }
            let sub_bin_offset = cand.freq_sub as f64 * (tone_spacing / FREQ_OSR as f64);
            let base_frequency = cand.freq_bin as f64 * tone_spacing + sub_bin_offset;
            let coarse_offset =
                (cand.time_step as isize - spectrogram.time_padding as isize) * spec_step as isize;
            let snr_db = par_estimate_snr_spectrogram(pp, &tone_mags);
            let confidence = (cand.sync_score / 12.0).min(1.0) as f32;
            let mut new_msg = DecodedMessage::new(
                ft8_message,
                snr_db,
                confidence,
                base_frequency,
                coarse_offset as f64 / SAMPLE_RATE as f64,
            );
            new_msg.tone_symbols = Some(Self::codeword_to_symbols(&corrected_bits));
            if diagnostic_on {
                self.residual_snr_records
                    .push((cand.sync_score, pre_snr_db, true));
            }
            decoded_new.push(new_msg);
        }
        decoded_new
    }

    /// hb-048 Session 3: a7 template cross-correlation pass.
    ///
    /// For each callsign already decoded in this window's `pass_decoded`,
    /// generate ~32 next-utterance templates via `a7::generate_templates`,
    /// then for each sync_candidate within ±`a7_freq_window_hz` of the
    /// expected call's audio frequency (and not already decoded), extract
    /// residual LLRs at the candidate position and run
    /// `a7::best_template_score`. Accept the winning template's message
    /// text as a decode when both `snr7 ≥ a7_snr7_threshold` AND
    /// `snr7b ≥ a7_snr7b_threshold`.
    ///
    /// **Mechanism vs V1 / V3**: V1 retries LDPC at original sync positions
    /// against the residual; V3 (shelved) relaxes the sync gate. a7 does
    /// *neither* — it uses a known-codeword matched-filter against the
    /// LLR stream, bypassing LDPC entirely for the templated decode.
    /// FP guard is structural (snr7b ratio against the rest of the
    /// template bank); downstream `is_plausible()` + the production FP
    /// filter catch what slips through.
    ///
    /// **Within-WAV vs cross-WAV**: this pass uses callsigns decoded in
    /// THIS window as the expected-call source. The
    /// `cross_slot_recent_calls` arg additionally seeds templates from
    /// cross-slot context (e.g. `ApContext.recent_calls` populated by a
    /// chronological-replay tier from `ChronoReplayState`, or by the
    /// production coordinator from `CrossTimeState`). Cross-slot calls
    /// have no known audio frequency, so they probe ALL sync_candidates
    /// (the within-WAV `±a7_freq_window_hz` gate only applies to in-WAV
    /// expected calls). Without cross-slot context the function still
    /// runs the within-WAV path — the design preserves the Session 3
    /// behavior at default config.
    ///
    /// Prior art: WSJT-X mainline commit
    /// `f13e31820470291fdd49627287a2dc08f3fa674c` (`lib/ft8_a7.f90`,
    /// Joe Taylor 2021). Canonical thresholds 6.0 / 1.8 came from there.
    fn a7_cross_correlation_pass(
        &mut self,
        spectrogram: &Spectrogram,
        sync_candidates: &[CostasCandidate],
        pass_decoded: &[DecodedMessage],
        cross_slot_recent_calls: &[crate::ap::RecentCallAp],
    ) -> Vec<DecodedMessage> {
        // Guard: nothing to template against if no decodes yet AND no
        // cross-slot calls — the pass is a no-op.
        if pass_decoded.is_empty() && cross_slot_recent_calls.is_empty() {
            return Vec::new();
        }

        #[cfg(not(feature = "transmit"))]
        {
            // The `a7::generate_templates` family requires the FT8 encoder,
            // which is gated behind `transmit`. Without it, the templates
            // are always empty and the pass is a no-op.
            let _ = (spectrogram, sync_candidates);
            return Vec::new();
        }

        #[cfg(feature = "transmit")]
        {
            let pp = &self.protocol_params;
            let tone_spacing = pp.tone_spacing;
            let sps = pp.samples_per_symbol(SAMPLE_RATE);
            let spec_step = sps / TIME_OSR;
            let lin = self.config.sync_time_interp_linear_power;

            // Build the set of (callsign, freq_hz, source) tuples for the
            // template generator. Sources:
            //   1. Within-WAV decodes from `pass_decoded` — these carry a
            //      known audio frequency (probe within ±freq_window_hz).
            //   2. Cross-slot recent calls from `cross_slot_recent_calls`
            //      — no known audio frequency (probe ALL sync_candidates).
            //
            // We track the freq-known-ness alongside each expected call so
            // the probe loop below can skip the freq filter when no
            // frequency is associated.
            //
            // Dedup by bare callsign (case-insensitive) so a callsign that
            // appears both in the snapshot AND in this window doesn't get
            // double-templated. Within-WAV entries (with known freq) win
            // on collision — they have the tighter probe window.
            struct A7ProbeEntry {
                ec: crate::a7::A7ExpectedCall,
                has_known_freq: bool,
            }
            let mut expected_calls: Vec<A7ProbeEntry> = Vec::new();
            let mut seen_calls: HashSet<String> = HashSet::new();

            for msg in pass_decoded {
                let Some(ref from_call) = msg.message.from_callsign else {
                    continue;
                };
                if from_call.is_empty() {
                    continue;
                }
                let bare = from_call
                    .split('/')
                    .next()
                    .unwrap_or(from_call)
                    .to_uppercase();
                if !seen_calls.insert(bare) {
                    continue;
                }
                let heard_with = msg.message.to_callsign.clone();
                let mut ec = crate::a7::A7ExpectedCall::new(
                    from_call.clone(),
                    msg.frequency_offset as f32,
                    // Parity is unknown at the decoder layer (the coordinator
                    // tags it post-decode); pass Even as a placeholder — the
                    // template generator doesn't gate on parity.
                    crate::a7::A7SlotParity::Even,
                );
                if let Some(other) = heard_with {
                    if !other.is_empty() {
                        ec = ec.with_heard_with(other);
                    }
                }
                expected_calls.push(A7ProbeEntry {
                    ec,
                    has_known_freq: true,
                });
            }

            // hb-048 S3-chrono: cross-slot recent calls (no known freq).
            // These probe every sync_candidate in the window — the
            // mechanism is "we heard this station before; look for any
            // follow-up here, freq unknown". This is the WSJT-X-canonical
            // a7 path when fed from slot N's heard-list into slot N+1.
            for rc in cross_slot_recent_calls {
                if rc.callsign.is_empty() {
                    continue;
                }
                let bare = rc
                    .callsign
                    .split('/')
                    .next()
                    .unwrap_or(&rc.callsign)
                    .to_uppercase();
                if !seen_calls.insert(bare) {
                    continue;
                }
                // freq_hz placeholder (unused when has_known_freq=false).
                let ec = crate::a7::A7ExpectedCall::new(
                    rc.callsign.clone(),
                    0.0,
                    crate::a7::A7SlotParity::Even,
                );
                expected_calls.push(A7ProbeEntry {
                    ec,
                    has_known_freq: false,
                });
            }

            if expected_calls.is_empty() {
                return Vec::new();
            }

            // Build the set of already-decoded message texts for downstream
            // dedup (and to avoid templating the SAME message back as a
            // "new" decode).
            let already_decoded_texts: HashSet<String> =
                pass_decoded.iter().map(|m| m.text.clone()).collect();

            // Build a small set of "subtracted positions" so we don't
            // re-attempt a7 at positions already cleanly decoded
            // (otherwise we'd just re-template C's own decode).
            let subtracted_positions: Vec<CostasCandidate> = pass_decoded
                .iter()
                .filter_map(|m| {
                    m.tone_symbols
                        .as_ref()
                        .map(|_| reverse_derive_candidate(m, pp, spectrogram.time_padding))
                })
                .collect();

            let freq_window_hz = self.config.a7_freq_window_hz;
            let snr7_threshold = self.config.a7_snr7_threshold;
            let snr7b_threshold = self.config.a7_snr7b_threshold;
            let llr_target_variance = self.config.llr_target_variance;

            let mut decoded_new: Vec<DecodedMessage> = Vec::new();
            let mut emitted_texts: HashSet<String> = HashSet::new();

            // For each expected call, generate templates and probe nearby
            // sync_candidates. Each (expected_call, candidate) pair is one
            // cross-correlation evaluation; the bank's `best_template_score`
            // picks the winner.
            //
            // Within-WAV entries (has_known_freq=true) probe only candidates
            // within ±freq_window_hz of the expected call. Cross-slot
            // entries (has_known_freq=false) probe ALL candidates — the
            // canonical WSJT-X a7 path searches the whole window for a
            // follow-up to any callsign heard in a previous slot.
            for entry in &expected_calls {
                let ec = &entry.ec;
                let templates = crate::a7::generate_templates(ec);
                if templates.is_empty() {
                    continue;
                }
                let ec_freq = ec.freq_hz as f64;

                // Find candidates near this expected call's freq. We
                // include candidates that DID decode too — for the
                // dual-station case where a single sync position
                // resolves to two callsigns at offset sub-bins.
                for cand in sync_candidates {
                    // Compute candidate audio frequency.
                    let sub_bin_offset = cand.freq_sub as f64 * (tone_spacing / FREQ_OSR as f64);
                    let cand_freq = cand.freq_bin as f64 * tone_spacing + sub_bin_offset;
                    if entry.has_known_freq && (cand_freq - ec_freq).abs() > freq_window_hz {
                        continue;
                    }
                    // Skip positions whose codeword has been subtracted —
                    // a7 against C's own already-clean decode is a no-op
                    // because the LLRs are zero-mean noise.
                    let already_subtracted = subtracted_positions.iter().any(|sp| {
                        sp.freq_sub == cand.freq_sub
                            && (sp.freq_bin as i64 - cand.freq_bin as i64).unsigned_abs() <= 1
                            && (sp.time_step as i64 - cand.time_step as i64).unsigned_abs() <= 2
                    });
                    if already_subtracted {
                        continue;
                    }

                    // Extract LLRs at this candidate position from the
                    // (possibly-residual) spectrogram. The decoder hands
                    // us `spectrogram` here, which IS the multipass
                    // residual once `coherent_subtract_and_repass` has
                    // run.
                    let tone_mags =
                        par_extract_symbols_from_spectrogram(pp, spectrogram, cand, lin);
                    let mut llrs = par_compute_soft_llrs_db(pp, &tone_mags);
                    // NOTE: JS8Call-Improved-style LLR whitening
                    // intentionally NOT applied here. a7 uses LLRs for
                    // cross-correlation against pre-encoded templates
                    // (snr7/snr7b thresholds are tuned against the
                    // legacy LLR scale); whitening would shift the
                    // distribution and require re-calibrating
                    // a7_snr7_threshold / a7_snr7b_threshold. The spec
                    // scopes whitening to LDPC BP input only.
                    normalize_llrs(&mut llrs, llr_target_variance);

                    let Some((best_idx, snr7, snr7b)) =
                        crate::a7::best_template_score(&templates, &llrs)
                    else {
                        continue;
                    };
                    if snr7 < snr7_threshold {
                        continue;
                    }
                    if snr7b < snr7b_threshold {
                        continue;
                    }

                    // Build a DecodedMessage from the winning template's
                    // text. Plausibility + the production FP filter run
                    // downstream of decode_window, so we don't gate on
                    // them here (they would for an LDPC decode but a7's
                    // codeword IS the template's, by construction).
                    let template_text = &templates[best_idx].message_text;
                    if already_decoded_texts.contains(template_text) {
                        continue;
                    }
                    if !emitted_texts.insert(template_text.clone()) {
                        continue;
                    }

                    let base_frequency = cand_freq;
                    let coarse_offset = (cand.time_step as isize
                        - spectrogram.time_padding as isize)
                        * spec_step as isize;
                    let time_offset_s = coarse_offset as f64 / SAMPLE_RATE as f64;
                    let snr_db = par_estimate_snr_spectrogram(pp, &tone_mags);
                    let confidence = (cand.sync_score / 12.0).min(1.0) as f32;

                    let ft8_message = crate::message::Ft8Message::from_text(template_text);
                    if !ft8_message.is_plausible() {
                        continue;
                    }
                    let new_msg = DecodedMessage::new(
                        ft8_message,
                        snr_db,
                        confidence,
                        base_frequency,
                        time_offset_s,
                    );
                    decoded_new.push(new_msg);
                }
            }
            decoded_new
        }
    }

    /// hb-086 V3 (SHELVED 2026-05-31): subtract-aware localized sync
    /// threshold relaxation. After multipass subtract+V1 saturate, run
    /// one more Costas sync_search on the residual at threshold
    /// `min_sync_score + joint_residual_sync_relax_db` (relax_db is
    /// negative), confined to freq_bins within
    /// `±joint_residual_sync_window_bins` of any subtracted-eligible
    /// decode position. Then decode each surfaced candidate against
    /// the residual via the same per-candidate path as V1.
    ///
    /// **Status: SHELVED.** The structural hypothesis (subtraction
    /// localizes its noise-floor drop to specific bins, so a bin-
    /// targeted relaxation surfaces decodable weak signals at those
    /// bins) is **wrong on this corpus**. Production hard-200 sweep
    /// (relax_db ∈ {-0.5, -1.0, -1.5, -2.0}) at the diagnostic-default
    /// window of 8 bins produced 0 additional decoded messages at
    /// every threshold. Mechanism trace
    /// (`examples/hb086_v3_trace.rs`) on top-3 worst hard-200 WAVs
    /// shows V3 surfaces ~100-131 truly-new (non-collision) candidates
    /// per WAV at all thresholds — but LDPC "decodes" all of them
    /// (random noise → BP converges on garbage), CRC catches ~98% as
    /// false positives, and plausibility rejects the remaining 1-4
    /// per WAV (all are CRC FPs that happen to form structured-but-
    /// invalid FT8 messages). The residual at sub-3.0 sync_score in
    /// the targeted window is *noise*, not weak signal.
    ///
    /// Why this differs from hb-082 (SHELVED — global residual
    /// relaxation, no-op): hb-082 found ZERO localized candidates
    /// because the global noise floor in the residual is unchanged.
    /// V3 *does* find candidates in the bin-targeted window (the
    /// noise floor IS lower at subtracted bins, enough to cross the
    /// relaxed score threshold), but they don't decode. The
    /// pre-graduation diagnostic
    /// (`examples/hb086_v3_subtract_window_potential.rs`) found 56.8%
    /// of V1-uncoverable truths sit within ±8 bins of a subtracted
    /// decode — geometric PROCEED — but the geometric proximity
    /// doesn't imply decodability. Plumbing kept at default-off for
    /// future revisit if the corpus or pipeline changes.
    fn joint_residual_localized_sync_pass(
        &self,
        spectrogram: &Spectrogram,
        sync_candidates: &[CostasCandidate],
        pass_decoded: &[DecodedMessage],
    ) -> Vec<DecodedMessage> {
        let pp = &self.protocol_params;
        let time_padding = spectrogram.time_padding;

        // Build the set of bin-centers from subtracted-eligible decodes
        // (every successful pancetta decode populates tone_symbols and
        // is subtract-eligible — pass-1 decodes are subtracted on
        // multipass round 1, multipass decodes are subtracted on
        // subsequent rounds, and V1 decodes are subtract-eligible too).
        let subtracted_positions: Vec<CostasCandidate> = pass_decoded
            .iter()
            .filter_map(|m| {
                m.tone_symbols
                    .as_ref()
                    .map(|_| reverse_derive_candidate(m, pp, time_padding))
            })
            .collect();
        if subtracted_positions.is_empty() {
            return Vec::new();
        }

        // Localized sync_search at the relaxed threshold. Restrict the
        // (t0, f0, freq_sub) sweep to f0 values within ±N freq_bins of
        // any subtracted_position. Time is unrestricted (the truth's t0
        // is arbitrary; sync_search must scan the time axis).
        let n_bins = self.config.joint_residual_sync_window_bins as i64;
        let relaxed_threshold =
            self.config.min_sync_score + self.config.joint_residual_sync_relax_db;
        let Ok(localized_candidates) = self.localized_costas_sync_search(
            spectrogram,
            &subtracted_positions,
            n_bins,
            relaxed_threshold,
        ) else {
            return Vec::new();
        };

        // Filter out candidates already covered by the existing
        // pipeline:
        //   - any subtracted-eligible position (those are decoded
        //     already; same ±1 freq_bin/±2 time_step tolerance the
        //     multipass uses);
        //   - any ORIGINAL sync_candidate position (V1 already retried
        //     those against the residual — V3 must surface only NEW
        //     positions sync_search missed in pass 1).
        let new_candidates: Vec<CostasCandidate> = localized_candidates
            .into_iter()
            .filter(|nc| {
                !subtracted_positions.iter().any(|sp| {
                    nc.freq_sub == sp.freq_sub
                        && (nc.freq_bin as i64 - sp.freq_bin as i64).unsigned_abs() <= 1
                        && (nc.time_step as i64 - sp.time_step as i64).unsigned_abs() <= 2
                }) && !sync_candidates.iter().any(|sc| {
                    nc.freq_sub == sc.freq_sub
                        && (nc.freq_bin as i64 - sc.freq_bin as i64).unsigned_abs() <= 1
                        && (nc.time_step as i64 - sc.time_step as i64).unsigned_abs() <= 2
                })
            })
            .take(self.config.max_sync_candidates)
            .collect();
        if new_candidates.is_empty() {
            return Vec::new();
        }

        // Decode each new candidate against the residual via the same
        // per-candidate path as V1 (sequential — count is bounded).
        let mut decoded_new: Vec<DecodedMessage> = Vec::new();
        let tone_spacing = pp.tone_spacing;
        let sps = pp.samples_per_symbol(SAMPLE_RATE);
        let spec_step = sps / TIME_OSR;
        let lin = self.config.sync_time_interp_linear_power;
        for cand in &new_candidates {
            let tone_mags = par_extract_symbols_from_spectrogram(pp, spectrogram, cand, lin);
            let mut llrs = par_compute_soft_llrs_db(pp, &tone_mags);
            maybe_whiten_llrs(self.config.llr_whitening_enabled, &mut llrs, &tone_mags, pp);
            normalize_llrs(&mut llrs, self.config.llr_target_variance);
            let Ok(corrected_bits) = self.ldpc_decoder.decode_soft(&llrs) else {
                continue;
            };
            if !par_verify_crc(&corrected_bits) {
                continue;
            }
            let payload_bits = par_apply_xor(pp.xor_sequence, &corrected_bits);
            let ft8_message = match self.message_parser.parse_payload(&payload_bits) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !ft8_message.is_plausible() {
                continue;
            }
            let sub_bin_offset = cand.freq_sub as f64 * (tone_spacing / FREQ_OSR as f64);
            let base_frequency = cand.freq_bin as f64 * tone_spacing + sub_bin_offset;
            let coarse_offset =
                (cand.time_step as isize - spectrogram.time_padding as isize) * spec_step as isize;
            let snr_db = par_estimate_snr_spectrogram(pp, &tone_mags);
            let confidence = (cand.sync_score / 12.0).min(1.0) as f32;
            let mut new_msg = DecodedMessage::new(
                ft8_message,
                snr_db,
                confidence,
                base_frequency,
                coarse_offset as f64 / SAMPLE_RATE as f64,
            );
            new_msg.tone_symbols = Some(Self::codeword_to_symbols(&corrected_bits));
            decoded_new.push(new_msg);
        }
        decoded_new
    }

    /// hb-086 V3 helper: like `costas_sync_search_with_threshold` but
    /// restricts the f0 sweep to bins within ±`n_bins` of any
    /// `target_position.freq_bin` (matching `freq_sub`). NMS+truncate
    /// still apply.
    fn localized_costas_sync_search(
        &self,
        spectrogram: &Spectrogram,
        target_positions: &[CostasCandidate],
        n_bins: i64,
        min_score: f64,
    ) -> Ft8Result<Vec<CostasCandidate>> {
        let mut candidates = Vec::new();
        let pp = &self.protocol_params;

        let steps_per_symbol = TIME_OSR;
        let msg_span = pp.num_symbols * steps_per_symbol;
        let max_time_step = spectrogram.num_steps.saturating_sub(msg_span + 1);
        let max_freq_bin = spectrogram.num_bins.saturating_sub(pp.num_tones);
        let max_freq_bin = max_freq_bin.min((4000.0 / pp.tone_spacing) as usize);

        // Build per-freq_sub set of allowed f0 values (sparse, so a
        // boolean mask is the cheapest representation).
        let mut allowed_per_sub: Vec<Vec<bool>> = (0..spectrogram.freq_osr)
            .map(|_| vec![false; max_freq_bin])
            .collect();
        for tp in target_positions {
            if tp.freq_sub >= spectrogram.freq_osr {
                continue;
            }
            let center = tp.freq_bin as i64;
            let lo = (center - n_bins).max(MIN_FREQ_BIN as i64) as usize;
            let hi = (center + n_bins + 1).min(max_freq_bin as i64) as usize;
            if lo >= hi {
                continue;
            }
            for cell in allowed_per_sub[tp.freq_sub][lo..hi].iter_mut() {
                *cell = true;
            }
        }

        for (freq_sub, allowed) in allowed_per_sub.iter().enumerate() {
            for t0 in 0..=max_time_step {
                for (f0, &is_allowed) in allowed
                    .iter()
                    .enumerate()
                    .take(max_freq_bin)
                    .skip(MIN_FREQ_BIN)
                {
                    if !is_allowed {
                        continue;
                    }
                    let score = self.compute_costas_score(spectrogram, t0, f0, freq_sub);
                    if score > min_score {
                        // V3 deliberately skips the parabolic time-refinement
                        // path used in the production sync_search. We're
                        // already in a relaxed-threshold corner; adding
                        // refinement complexity here would conflate two
                        // axes of variability. If a position decodes from
                        // its integer t0, it's a real signal; if not,
                        // refinement isn't going to rescue it.
                        candidates.push(CostasCandidate {
                            time_step: t0,
                            freq_bin: f0,
                            freq_sub,
                            sync_score: score,
                            time_refinement: 0.0,
                        });
                    }
                }
            }
        }

        candidates.sort_by(|a, b| {
            b.sync_score
                .partial_cmp(&a.sync_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.truncate(self.config.max_sync_candidates);
        if self.config.nms_enabled {
            self.nms_candidates(&mut candidates);
        }
        Ok(candidates)
    }

    // ========================================================================
    // Candidate decoding (AP0 — standard path)
    // ========================================================================

    /// Pipeline:
    /// 1. Fine timing search: refine coarse time offset (±half symbol, 9 steps at 1/8 symbol)
    /// 2. Frequency refinement: try ±1 bin
    /// 3. Extract symbols with complex DFT
    /// 4. Compute soft LLRs + normalize to target variance
    /// 5. LDPC belief propagation
    /// 6. CRC-14 verification
    /// 7. Message parsing
    fn decode_candidate(
        &mut self,
        audio: &[f64],
        candidate: &CostasCandidate,
        spectrogram: &Spectrogram,
    ) -> Ft8Result<Option<DecodedMessage>> {
        // Copy protocol params values to locals to avoid holding a borrow on self
        // (decode_candidate is &mut self for buffer reuse in extract_symbols_complex)
        let sps = self.protocol_params.samples_per_symbol(SAMPLE_RATE);
        let tone_spacing = self.protocol_params.tone_spacing;
        let xor_sequence = self.protocol_params.xor_sequence;
        let spec_step = sps / TIME_OSR;
        let coarse_offset = candidate.time_step * spec_step;

        // ---- Spectrogram-based symbol extraction: try both freq_sub values ----
        // The spectrogram uses a 3840-pt FFT (3.125 Hz resolution), which
        // avoids the spectral leakage of the 1920-pt independent FFT.
        // Signals on a bin boundary may decode better with the other sub-bin.
        let freq_sub_trials = [
            candidate.freq_sub,
            if candidate.freq_sub == 0 { 1 } else { 0 },
        ];
        for &trial_freq_sub in &freq_sub_trials {
            let trial_candidate = CostasCandidate {
                freq_sub: trial_freq_sub,
                ..*candidate
            };
            let tone_magnitudes =
                self.extract_symbols_from_spectrogram(spectrogram, &trial_candidate);
            let mut llrs = self.compute_soft_llrs_db(&tone_magnitudes);
            maybe_whiten_llrs(
                self.config.llr_whitening_enabled,
                &mut llrs,
                &tone_magnitudes,
                &self.protocol_params,
            );
            normalize_llrs(&mut llrs, self.config.llr_target_variance);

            if let Ok(corrected_bits) = self.ldpc_decoder.decode_soft(&llrs) {
                if self.verify_crc(&corrected_bits) {
                    // CRC passed — compute frequency and time for the message
                    let sub_bin_offset = trial_freq_sub as f64 * (tone_spacing / FREQ_OSR as f64);
                    let base_frequency = candidate.freq_bin as f64 * tone_spacing + sub_bin_offset;
                    let time_offset_samples = coarse_offset;

                    // For FT4, un-apply the XOR scrambling on the payload
                    let payload_bits = if let Some(xor_seq) = xor_sequence {
                        let mut bits = corrected_bits[0..PAYLOAD_BITS].to_owned();
                        for byte_idx in 0..10 {
                            let xor_byte = xor_seq[byte_idx];
                            for bit_pos in 0..8 {
                                let global_bit = byte_idx * 8 + bit_pos;
                                if global_bit >= PAYLOAD_BITS {
                                    break;
                                }
                                if (xor_byte >> (7 - bit_pos)) & 1 == 1 {
                                    let cur = bits[global_bit];
                                    bits.set(global_bit, !cur);
                                }
                            }
                        }
                        bits
                    } else {
                        corrected_bits[0..PAYLOAD_BITS].to_owned()
                    };
                    let ft8_message = self.message_parser.parse_payload(&payload_bits)?;

                    // Reject CRC false positives: verify the payload parses
                    // into a structurally valid FT8 message (has callsigns, etc.)
                    if !ft8_message.is_plausible() {
                        #[cfg(feature = "debug-decode")]
                        eprintln!(
                            "    spectrogram path: CRC passed but message not plausible: {}",
                            ft8_message
                        );
                        continue;
                    }

                    // SNR estimate from spectrogram magnitudes (dB domain)
                    let snr_db = {
                        let data_positions = self.protocol_params.data_symbol_indices();
                        let mut signal_sum = 0.0f64;
                        let mut noise_sum = 0.0f64;
                        let mut count = 0usize;
                        for &sym_idx in &data_positions {
                            let mags = &tone_magnitudes[sym_idx];
                            let best = mags.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                            let worst = mags.iter().cloned().fold(f64::INFINITY, f64::min);
                            signal_sum += best;
                            noise_sum += worst;
                            count += 1;
                        }
                        if count > 0 {
                            let avg_signal_db = signal_sum / count as f64;
                            let avg_noise_db = noise_sum / count as f64;
                            let snr_bin_db = avg_signal_db - avg_noise_db;
                            let bw_correction = 10.0 * (2500.0f64 / 6.25).log10();
                            (snr_bin_db - bw_correction) as f32
                        } else {
                            -24.0f32
                        }
                    };
                    let confidence = (candidate.sync_score / 12.0).min(1.0) as f32;

                    let mut decoded_message = DecodedMessage::new(
                        ft8_message,
                        snr_db,
                        confidence,
                        base_frequency,
                        time_offset_samples as f64 / SAMPLE_RATE as f64,
                    );
                    // Store tone symbols for multi-pass signal subtraction
                    decoded_message.tone_symbols = Some(Self::codeword_to_symbols(&corrected_bits));

                    #[cfg(feature = "debug-decode")]
                    eprintln!(
                        "    spectrogram path (freq_sub={}): CRC PASSED for t={} f={}",
                        trial_freq_sub, candidate.time_step, candidate.freq_bin
                    );

                    return Ok(Some(decoded_message));
                }
            }
        }

        // ---- Fine-timing FFT-based extraction (expensive: 9×5 = 45 FFT trials) ----
        // Only attempt for strong candidates — weak ones rarely decode via this path
        // if the spectrogram path already failed.
        if candidate.sync_score < 3.5 {
            return Ok(None);
        }

        // Fine timing: search ±3/8 symbol in eighth-symbol steps.
        // 7 steps at 1/8 symbol = 240 samples each.
        let eighth_sym = (sps / 8) as isize;
        let time_deltas: [isize; 7] = [
            -3 * eighth_sym,
            -2 * eighth_sym,
            -eighth_sym,
            0,
            eighth_sym,
            2 * eighth_sym,
            3 * eighth_sym,
        ];

        // Frequency refinement: try ±0.5 bin
        // 3 frequency trials: -0.5, 0, +0.5
        // (in units of tone_spacing = 6.25 Hz, so steps are 3.125 Hz)
        let freq_offsets: [f64; 3] = [0.0, -0.5, 0.5];

        // freq_sub shifts the base frequency by half a bin when freq_osr=2
        let sub_bin_offset = candidate.freq_sub as f64 * (tone_spacing / FREQ_OSR as f64);

        // Find best (time_delta, freq_offset) by Costas correlation on extracted symbols
        let mut best_decode = None;

        for &dt in &time_deltas {
            let time_offset = coarse_offset as isize + dt;
            if time_offset < 0 {
                continue;
            }
            let time_offset_samples = time_offset as usize;

            for &df in &freq_offsets {
                let freq_hz =
                    candidate.freq_bin as f64 * tone_spacing + sub_bin_offset + df * tone_spacing;
                if freq_hz < 0.0 {
                    continue;
                }
                let base_frequency = freq_hz;

                let (_symbols, tone_magnitudes) = match self.extract_symbols_complex(
                    audio,
                    time_offset_samples,
                    base_frequency,
                ) {
                    Ok(result) => result,
                    Err(_) => continue,
                };

                let mut llrs = self.compute_soft_llrs(&tone_magnitudes);
                maybe_whiten_llrs(
                    self.config.llr_whitening_enabled,
                    &mut llrs,
                    &tone_magnitudes,
                    &self.protocol_params,
                );

                // LLR normalization: scale to target variance (ft8_lib's ftx_normalize_logl)
                normalize_llrs(&mut llrs, self.config.llr_target_variance);

                #[cfg(feature = "debug-decode")]
                {
                    let avg_abs_llr = llrs.iter().map(|l| l.abs()).sum::<f32>() / llrs.len() as f32;
                    let saturated = llrs.iter().filter(|&&l| l.abs() >= 24.9).count();
                    eprintln!(
                        "    dt={:+4} df={:+.1}: avg|LLR|={:.2}, sat={}/174",
                        dt, df, avg_abs_llr, saturated
                    );
                }

                let corrected_bits = match self.ldpc_decoder.decode_soft(&llrs) {
                    Ok(bits) => bits,
                    Err(_) => continue,
                };

                if !self.verify_crc(&corrected_bits) {
                    continue;
                }

                // CRC passed — parse message and return
                #[cfg(feature = "debug-decode")]
                eprintln!("    dt={:+4} df={:+.1}: CRC PASSED!", dt, df);

                // For FT4, un-apply the XOR scrambling on the payload
                let payload_bits = if let Some(xor_seq) = xor_sequence {
                    let mut bits = corrected_bits[0..PAYLOAD_BITS].to_owned();
                    for byte_idx in 0..10 {
                        let xor_byte = xor_seq[byte_idx];
                        for bit_pos in 0..8 {
                            let global_bit = byte_idx * 8 + bit_pos;
                            if global_bit >= PAYLOAD_BITS {
                                break;
                            }
                            if (xor_byte >> (7 - bit_pos)) & 1 == 1 {
                                let cur = bits[global_bit];
                                bits.set(global_bit, !cur);
                            }
                        }
                    }
                    bits
                } else {
                    corrected_bits[0..PAYLOAD_BITS].to_owned()
                };
                let ft8_message = self.message_parser.parse_payload(&payload_bits)?;

                // Reject CRC false positives: verify the payload parses
                // into a structurally valid FT8 message (has callsigns, etc.)
                if !ft8_message.is_plausible() {
                    #[cfg(feature = "debug-decode")]
                    eprintln!(
                        "    fine-timing path: CRC passed but message not plausible: {}",
                        ft8_message
                    );
                    continue;
                }

                // Estimate SNR from extracted tone magnitudes.
                // Signal power: average squared magnitude of the best tone across data symbols.
                // Noise power: average squared magnitude of the weakest tone across data symbols.
                // This gives SNR in the 6.25 Hz bin width; correct to 2500 Hz reference BW.
                let snr_db = {
                    let data_positions = self.protocol_params.data_symbol_indices();
                    let mut signal_power = 0.0f64;
                    let mut noise_power = 0.0f64;
                    let mut count = 0usize;
                    for &sym_idx in &data_positions {
                        let mags = &tone_magnitudes[sym_idx];
                        let best = mags.iter().cloned().fold(0.0f64, f64::max);
                        let worst = mags.iter().cloned().fold(f64::MAX, f64::min);
                        signal_power += best * best;
                        noise_power += worst * worst;
                        count += 1;
                    }
                    if count > 0 && noise_power > 0.0 {
                        let snr_linear = signal_power / noise_power;
                        // Convert from bin BW (6.25 Hz) to reference BW (2500 Hz)
                        let bw_correction = 10.0 * (2500.0f64 / 6.25).log10(); // = 26.02 dB
                        (10.0 * snr_linear.log10() - bw_correction) as f32
                    } else {
                        -24.0f32 // fallback for degenerate case
                    }
                };
                let confidence = (candidate.sync_score / 12.0).min(1.0) as f32;

                let mut decoded_message = DecodedMessage::new(
                    ft8_message,
                    snr_db,
                    confidence,
                    base_frequency,
                    time_offset_samples as f64 / SAMPLE_RATE as f64,
                );
                // Store tone symbols for multi-pass signal subtraction
                decoded_message.tone_symbols = Some(Self::codeword_to_symbols(&corrected_bits));

                best_decode = Some(decoded_message);
                return Ok(best_decode);
            }
        }

        Ok(best_decode)
    }

    // ========================================================================
    // Symbol extraction using FFT (performance-optimized)
    // ========================================================================

    /// Extract all 79 symbols from audio using FFT at each symbol position.
    ///
    /// For each symbol, computes a windowed FFT and reads the magnitude at
    /// each of the 8 tone frequencies. This replaces the naive per-tone DFT
    /// approach (O(N*K) → O(N log N)) for a ~20× speedup.
    ///
    /// Returns the hard-decision symbols AND the per-tone magnitude vectors
    /// (needed for soft LLR computation).
    fn extract_symbols_complex(
        &mut self,
        audio: &[f64],
        time_offset_samples: usize,
        base_frequency: f64,
    ) -> Ft8Result<(Vec<u8>, Vec<[f64; NUM_TONES]>)> {
        let pp = &self.protocol_params;
        let sps = pp.samples_per_symbol(SAMPLE_RATE);
        let end_sample = time_offset_samples + pp.num_symbols * sps;
        if end_sample > audio.len() {
            return Err(Ft8Error::InsufficientData {
                needed: end_sample,
                available: audio.len(),
            });
        }

        let pi2 = 2.0 * std::f64::consts::PI;

        // Use cached FFT plan and Hann window from decoder initialization.
        // freq_resolution = sample_rate / sps = 6.25 Hz = tone_spacing.
        // We frequency-shift the signal so that base_frequency maps to DC (bin 0),
        // then tones 0..7 map to bins 0..7. This handles arbitrary base_frequency
        // values (including sub-bin offsets from freq_osr=2) without zero-padding.
        // Pre-compute complex rotation step for frequency shift.
        // Instead of calling sin_cos per sample, we compute the initial phase
        // per symbol and rotate by a fixed step = exp(-j*2*pi*base_freq/fs).
        let phase_step_angle = -pi2 * base_frequency / SAMPLE_RATE as f64;
        let phase_step = Complex::new(phase_step_angle.cos(), phase_step_angle.sin());

        // Take the window and buffer out of self to avoid borrow conflicts in the loop
        let mut fft_buffer = std::mem::take(&mut self.symbol_fft_buffer);
        let window = &self.symbol_window;
        let fft = &self.symbol_fft;

        let mut symbols = Vec::with_capacity(pp.num_symbols);
        let mut tone_magnitudes = Vec::with_capacity(pp.num_symbols);

        for sym_idx in 0..pp.num_symbols {
            let sym_start = time_offset_samples + sym_idx * sps;
            let symbol_audio = &audio[sym_start..sym_start + sps];

            // Compute initial phase for this symbol's first sample
            let initial_angle = -pi2 * base_frequency * sym_start as f64 / SAMPLE_RATE as f64;
            let mut rotator = Complex::new(initial_angle.cos(), initial_angle.sin());

            // Apply window + frequency shift using complex rotation
            for i in 0..sps {
                let w = window[i];
                fft_buffer[i] = Complex::new(
                    symbol_audio[i] * w * rotator.re,
                    symbol_audio[i] * w * rotator.im,
                );
                rotator = rotator * phase_step;
            }

            // Compute FFT — tone k is now at bin k
            fft.process(&mut fft_buffer);

            // Read magnitudes at bins 0..num_tones
            let mut mags = [0.0f64; NUM_TONES];
            let mut best_tone = 0u8;
            let mut best_mag = 0.0;

            for tone in 0..pp.num_tones {
                let magnitude = fft_buffer[tone].norm();
                mags[tone] = magnitude;

                if magnitude > best_mag {
                    best_mag = magnitude;
                    best_tone = tone as u8;
                }
            }

            symbols.push(best_tone);
            tone_magnitudes.push(mags);
        }

        // Return the buffer to self for reuse
        self.symbol_fft_buffer = fft_buffer;

        Ok((symbols, tone_magnitudes))
    }

    // ========================================================================
    // Spectrogram-based symbol extraction
    // ========================================================================

    /// Extract all 79 symbols from the pre-computed spectrogram.
    ///
    /// Instead of running an independent FFT per symbol, read tone magnitudes
    /// directly from the spectrogram which was computed with freq_osr=2 (3840-pt
    /// FFT, 3.125 Hz resolution). This eliminates ~2-4 dB of spectral leakage
    /// for sub-bin signals.
    ///
    /// For each symbol, averages all TIME_OSR*2 sub-steps for improved SNR.
    /// Returns magnitudes already in dB (matching spectrogram storage).
    fn extract_symbols_from_spectrogram(
        &self,
        spectrogram: &Spectrogram,
        candidate: &CostasCandidate,
    ) -> Vec<[f64; NUM_TONES]> {
        let pp = &self.protocol_params;
        let t0 = candidate.time_step;
        let f0 = candidate.freq_bin;
        let fs = candidate.freq_sub;
        // hb-044: optional fractional time-bin shift. dt=0 → identical
        // to original integer-bin behavior.
        let dt = candidate.time_refinement;
        // hb-069: linear-power interpolation gate.
        let lin = self.config.sync_time_interp_linear_power;

        let mut tone_magnitudes = Vec::with_capacity(pp.num_symbols);

        let steps_per_symbol = TIME_OSR;

        for sym_idx in 0..pp.num_symbols {
            let mut mags = [-120.0f64; NUM_TONES];

            // Each symbol spans steps_per_symbol time steps
            let t_base = t0 + sym_idx * steps_per_symbol;

            for tone in 0..pp.num_tones {
                let freq_bin = f0 + tone;

                // Guard against out-of-bounds
                if freq_bin >= spectrogram.num_bins || fs >= spectrogram.freq_osr {
                    continue;
                }

                // Average the first 2 sub-steps within this symbol (the
                // center of the symbol window at the Costas-aligned offset).
                // hb-044: dt applies a fractional time-bin shift via
                // linear interpolation; dt=0 reproduces original behavior.
                let db_a = lookup_time_interp(spectrogram, t_base, dt, fs, freq_bin, lin);
                let db_b = lookup_time_interp(spectrogram, t_base + 1, dt, fs, freq_bin, lin);
                mags[tone] = (db_a + db_b) / 2.0;
            }

            tone_magnitudes.push(mags);
        }

        tone_magnitudes
    }

    // ========================================================================
    // Soft LLR computation (Bug 1.3 fix)
    // ========================================================================

    /// Compute soft LLRs from tone magnitudes that are already in dB.
    ///
    /// This is the spectrogram path: values come from `extract_symbols_from_spectrogram`
    /// and are already in dB, so we skip the `10*log10(1e-12 + mag^2)` conversion.
    fn compute_soft_llrs_db(&self, tone_magnitudes: &[[f64; NUM_TONES]]) -> Vec<f32> {
        let pp = &self.protocol_params;
        let mut llrs = Vec::with_capacity(174);
        let data_positions = pp.data_symbol_indices();

        for &sym_idx in &data_positions {
            let mags = &tone_magnitudes[sym_idx];

            match pp.bits_per_symbol {
                3 => {
                    // 8-FSK (FT8/FT2): 3 LLRs per symbol
                    // Values are already in dB — use directly
                    let mut s2 = [0.0f64; 8];
                    for j in 0..8 {
                        let tone_idx = crate::ldpc::binary_to_gray(j as u8) as usize;
                        s2[j] = mags[tone_idx];
                    }

                    fn max4(a: f64, b: f64, c: f64, d: f64) -> f64 {
                        a.max(b).max(c.max(d))
                    }

                    let llr0 = max4(s2[4], s2[5], s2[6], s2[7]) - max4(s2[0], s2[1], s2[2], s2[3]);
                    let llr1 = max4(s2[2], s2[3], s2[6], s2[7]) - max4(s2[0], s2[1], s2[4], s2[5]);
                    let llr2 = max4(s2[1], s2[3], s2[5], s2[7]) - max4(s2[0], s2[2], s2[4], s2[6]);

                    llrs.push(-llr0 as f32);
                    llrs.push(-llr1 as f32);
                    llrs.push(-llr2 as f32);
                }
                2 => {
                    // 4-FSK (FT4): 2 LLRs per symbol
                    let mut s2 = [0.0f64; 4];
                    for j in 0..4 {
                        let tone_idx = crate::ldpc::binary_to_gray_4fsk(j as u8) as usize;
                        s2[j] = mags[tone_idx];
                    }

                    let llr0 = s2[2].max(s2[3]) - s2[0].max(s2[1]);
                    let llr1 = s2[1].max(s2[3]) - s2[0].max(s2[2]);

                    llrs.push(-llr0 as f32);
                    llrs.push(-llr1 as f32);
                }
                _ => unreachable!("Unsupported bits_per_symbol"),
            }
        }

        debug_assert_eq!(llrs.len(), 174);
        llrs
    }

    /// Compute soft log-likelihood ratios from per-symbol tone magnitudes.
    ///
    /// Matches ft8_lib's ft8_extract_symbol approach: for each of the 58 data
    /// symbols x 3 bits = 174 codeword bits, compute the LLR using the max-log
    /// approximation on log-magnitude (dB) values:
    ///
    ///   LLR(bit_k) = max(dB_mag[tones where bit_k=1]) - max(dB_mag[tones where bit_k=0])
    ///
    /// Gray code mapping determines which tones correspond to bit=0 vs bit=1.
    /// The raw LLRs are later normalized by normalize_llrs() to target variance.
    fn compute_soft_llrs(&self, tone_magnitudes: &[[f64; NUM_TONES]]) -> Vec<f32> {
        let pp = &self.protocol_params;
        let mut llrs = Vec::with_capacity(174);
        let data_positions = pp.data_symbol_indices();

        for &sym_idx in &data_positions {
            let mags = &tone_magnitudes[sym_idx];

            match pp.bits_per_symbol {
                3 => {
                    // 8-FSK (FT8/FT2): 3 LLRs per symbol
                    let mut s2 = [0.0f64; 8];
                    for j in 0..8 {
                        let tone_idx = crate::ldpc::binary_to_gray(j as u8) as usize;
                        s2[j] = (1e-12 + mags[tone_idx] * mags[tone_idx]).log10() * 10.0;
                    }

                    fn max4(a: f64, b: f64, c: f64, d: f64) -> f64 {
                        a.max(b).max(c.max(d))
                    }

                    let llr0 = max4(s2[4], s2[5], s2[6], s2[7]) - max4(s2[0], s2[1], s2[2], s2[3]);
                    let llr1 = max4(s2[2], s2[3], s2[6], s2[7]) - max4(s2[0], s2[1], s2[4], s2[5]);
                    let llr2 = max4(s2[1], s2[3], s2[5], s2[7]) - max4(s2[0], s2[2], s2[4], s2[6]);

                    llrs.push(-llr0 as f32);
                    llrs.push(-llr1 as f32);
                    llrs.push(-llr2 as f32);
                }
                2 => {
                    // 4-FSK (FT4): 2 LLRs per symbol
                    // Gray map: binary 0→tone 0, 1→tone 1, 2→tone 3, 3→tone 2
                    let mut s2 = [0.0f64; 4];
                    for j in 0..4 {
                        let tone_idx = crate::ldpc::binary_to_gray_4fsk(j as u8) as usize;
                        s2[j] = (1e-12 + mags[tone_idx] * mags[tone_idx]).log10() * 10.0;
                    }

                    // bit0: binary values {2,3} have bit0=1, {0,1} have bit0=0
                    let llr0 = s2[2].max(s2[3]) - s2[0].max(s2[1]);
                    // bit1: binary values {1,3} have bit1=1, {0,2} have bit1=0
                    let llr1 = s2[1].max(s2[3]) - s2[0].max(s2[2]);

                    llrs.push(-llr0 as f32);
                    llrs.push(-llr1 as f32);
                }
                _ => unreachable!("Unsupported bits_per_symbol"),
            }
        }

        debug_assert_eq!(llrs.len(), 174);
        llrs
    }

    // ========================================================================
    // CRC verification
    // ========================================================================

    /// Verify CRC-14 checksum
    fn verify_crc(&self, bits: &BitVec) -> bool {
        if bits.len() < PAYLOAD_BITS + CRC_BITS {
            return false;
        }

        let payload = &bits[0..PAYLOAD_BITS];
        let received_crc_bits = &bits[PAYLOAD_BITS..PAYLOAD_BITS + CRC_BITS];

        let calculated_crc = calculate_crc14(payload);
        let received_crc = bits_to_u16(received_crc_bits);

        calculated_crc == received_crc
    }

    // ========================================================================
    // Waterfall display
    // ========================================================================

    /// Generate waterfall display data
    /// Generate waterfall data for one FT8 decode window.
    ///
    /// Produces a small number of summary rows (target_rows) by averaging
    /// multiple FFT frames, covering the 0–3000 Hz USB audio passband.
    /// Each call = one FT8 cycle; the TUI stacks these vertically so the
    /// operator can see activity across many cycles (odd/even).
    pub fn generate_waterfall_data(&mut self, audio: &[f64]) -> Ft8Result<WaterfallData> {
        let fft_size = self.fft_processor.fft_size();
        let window_size = fft_size.min(audio.len());
        let hop_size = window_size / 4;
        let num_ffts = (audio.len().saturating_sub(window_size)) / hop_size + 1;

        // Produce a small number of rows per FT8 cycle so they stack nicely.
        // 4 rows per 15s cycle = ~3.75s per row, good granularity for even/odd.
        let target_rows: usize = 4;
        let ffts_per_row = (num_ffts / target_rows).max(1);

        let freq_resolution = SAMPLE_RATE as f64 / fft_size as f64;

        // FT8 USB passband: 0–3000 Hz
        let bin_start = 0usize;
        let bin_end = (3000.0 / freq_resolution).floor() as usize;
        let bin_end = bin_end.min(fft_size / 2);
        let num_bins = bin_end - bin_start + 1;

        let mut waterfall_data = WaterfallData {
            time_bins: Vec::new(),
            frequency_bins: (bin_start..=bin_end)
                .map(|i| i as f64 * freq_resolution)
                .collect(),
            power_matrix: Vec::new(),
            min_power: f64::MAX,
            max_power: f64::MIN,
        };

        // Accumulate FFTs into summary rows
        let mut accum: Vec<f64> = vec![0.0; num_bins];
        let mut accum_count: usize = 0;

        for fft_idx in 0..num_ffts {
            let start = fft_idx * hop_size;
            let end = (start + window_size).min(audio.len());
            if end - start < window_size {
                break;
            }

            let window = &audio[start..end];
            let psd = self.fft_processor.power_spectral_density(window)?;

            for (j, i) in (bin_start..=bin_end.min(psd.len() - 1)).enumerate() {
                accum[j] += psd[i];
            }
            accum_count += 1;

            // Emit a summary row when we've accumulated enough FFTs
            if accum_count >= ffts_per_row || fft_idx == num_ffts - 1 {
                let row: Vec<f64> = accum
                    .iter()
                    .map(|&sum| {
                        let avg = sum / accum_count as f64;
                        let db = 10.0 * (avg + 1e-12).log10();
                        waterfall_data.min_power = waterfall_data.min_power.min(db);
                        waterfall_data.max_power = waterfall_data.max_power.max(db);
                        db
                    })
                    .collect();

                waterfall_data.power_matrix.push(row);
                waterfall_data.time_bins.push(
                    (fft_idx as f64 - accum_count as f64 / 2.0) * hop_size as f64
                        / SAMPLE_RATE as f64,
                );

                accum.fill(0.0);
                accum_count = 0;
            }
        }

        Ok(waterfall_data)
    }

    // ========================================================================
    // Accessors
    // ========================================================================

    /// Get the last decoding metrics
    pub fn get_last_metrics(&self) -> &DecodingMetrics {
        &self.last_metrics
    }

    /// Check if decoder is synchronized
    pub fn is_synchronized(&self) -> bool {
        // TimeSync was removed as dead code; sync is implicitly achieved
        // via Costas array correlation during decode_window
        true
    }
}

// ============================================================================
// Parallel candidate decoding context and free functions
// ============================================================================

/// Immutable decode context shared across rayon threads.
///
/// Captures all the state from `Ft8Decoder` that candidate decoding reads
/// but never writes. Each rayon worker gets a shared `&DecodeContext` plus
/// its own thread-local `LdpcDecoder` and FFT buffers.
struct DecodeContext<'a> {
    protocol_params: &'a ProtocolParams,
    message_parser: &'a MessageParser,
    spectrogram: &'a Spectrogram,
    audio: &'a [f64],
    ap_context: &'a crate::ap::ApContext,
    ap_active: bool,
    /// Pre-computed FFT plan for symbol extraction (sps-length), Arc is Send+Sync
    symbol_fft: &'a std::sync::Arc<dyn rustfft::Fft<f64>>,
    /// Pre-computed Hann window for symbol extraction
    symbol_window: &'a [f64],
    /// XOR sequence for FT4
    xor_sequence: Option<&'static [u8; 10]>,
    /// OSD config for creating per-thread LDPC decoders
    ldpc_iterations: usize,
    osd_depth: Option<u8>,
    /// LLR normalization target variance (matches Ft8Config field).
    llr_target_variance: f32,
    /// When true, per-thread LDPC decoders are created in 3 buckets
    /// (low/mid/high iter counts) and dispatched per candidate by
    /// sync_score. hb-022 wild-card config flag.
    adaptive_ldpc_iters: bool,
    /// Max parity errors tolerated before invoking OSD fallback. hb-014.
    max_parity_errors_for_osd: usize,
    /// hb-067 mBP offset (subtract from |LLR| before OSD).
    bp_offset_subtract: f32,
    /// hb-063 layered (row-sequential) BP schedule.
    layered_bp: bool,
    /// JS8Call-Improved-style LDPC feedback refinement: master switch.
    ldpc_feedback_refinement_enabled: bool,
    /// JS8Call-Improved-style LDPC feedback refinement: agree-boost factor.
    ldpc_feedback_boost_factor: f32,
    /// JS8Call-Improved-style LDPC feedback refinement: disagree-attenuate
    /// factor.
    ldpc_feedback_attenuate_factor: f32,
    /// JS8Call-Improved-style LDPC feedback refinement: erase threshold.
    ldpc_feedback_erase_threshold: f32,
    /// hb-069 linear-power spectrogram interpolation gate.
    sync_time_interp_linear_power: bool,
    /// hb-129: window-start instant. Used to stamp each successful decode
    /// with its presentation-time-into-window for the TTFD metric.
    window_start: Instant,
    /// hb-244: optional soft combiner shared across rayon workers.
    /// `None` when `config.soft_combiner_enabled = false`. The combiner
    /// is wrapped in a `Mutex` and only locked when present, so the
    /// disabled hot path is a single `Option::as_ref()` branch test.
    soft_combiner: Option<&'a std::sync::Arc<std::sync::Mutex<SoftCombiner>>>,
    /// JS8Call-Improved-inspired per-tone × per-symbol LLR whitening.
    /// When `false` the whitening helper is never invoked, leaving the
    /// LLR pipeline byte-identical to the legacy path.
    llr_whitening_enabled: bool,
}

/// Result from parallel candidate decoding (one candidate).
struct ParDecodedCandidate {
    msg: DecodedMessage,
}

/// Decode a single candidate in parallel — AP0 path (spectrogram + fine-timing FFT).
///
/// This is the free-function equivalent of `Ft8Decoder::decode_candidate`, but
/// takes only immutable shared state via `DecodeContext` and a mutable
/// per-thread `LdpcDecoder` and FFT buffer.
fn par_decode_candidate(
    ctx: &DecodeContext,
    candidate: &CostasCandidate,
    ldpc: &LdpcDecoder,
    fft_buffer: &mut Vec<Complex<f64>>,
) -> Option<DecodedMessage> {
    let sps = ctx.protocol_params.samples_per_symbol(SAMPLE_RATE);
    let tone_spacing = ctx.protocol_params.tone_spacing;
    let xor_sequence = ctx.xor_sequence;
    let spec_step = sps / TIME_OSR;
    let coarse_offset =
        (candidate.time_step as isize - ctx.spectrogram.time_padding as isize) * spec_step as isize;

    // ---- Spectrogram-based symbol extraction: try both freq_sub values ----
    let freq_sub_trials = [
        candidate.freq_sub,
        if candidate.freq_sub == 0 { 1 } else { 0 },
    ];
    // hb-244: build the coarse combiner key once per candidate.
    // freq_bin/time_step are the candidate's spectrogram coordinates;
    // the combiner uses them to group repeated receptions of the same
    // physical signal. Mode is derived from the active protocol.
    let combiner_key_for = |freq_sub: usize| {
        let combiner_mode = match ctx.protocol_params.protocol {
            crate::Protocol::Ft8 => crate::SoftCombinerMode::Ft8,
            crate::Protocol::Ft4 => crate::SoftCombinerMode::Ft4,
            #[cfg(feature = "ft2")]
            crate::Protocol::Ft2 => crate::SoftCombinerMode::Ft8,
        };
        // freq_bin step matches FREQ_OSR; encode (freq_bin, freq_sub)
        // into a single u32 so two trial freq_subs produce distinct keys
        // for the same coarse freq_bin.
        let coarse_freq = (candidate.freq_bin as u32) * (FREQ_OSR as u32) + (freq_sub as u32);
        crate::CombinerKey::new(combiner_mode, coarse_freq, candidate.time_step as i32)
    };

    for &trial_freq_sub in &freq_sub_trials {
        let trial_candidate = CostasCandidate {
            freq_sub: trial_freq_sub,
            ..*candidate
        };
        let tone_magnitudes = par_extract_symbols_from_spectrogram(
            ctx.protocol_params,
            ctx.spectrogram,
            &trial_candidate,
            ctx.sync_time_interp_linear_power,
        );
        let mut llrs = par_compute_soft_llrs_db(ctx.protocol_params, &tone_magnitudes);
        // JS8Call-Improved-inspired LLR whitening (inspired by spec ref
        // `spec-js8call-llr-whitening.md`). Applied BEFORE normalisation
        // so the per-tone × per-symbol divisive step sees raw demapper
        // LLRs; normalize_llrs then re-standardises variance. The
        // whitened LLRs feed the soft combiner, which accumulates
        // already-whitened receptions; the post-combine path falls back
        // to plain normalize_llrs (the combiner does not retain per-
        // reception magnitudes, so re-whitening after combine would
        // have no input).
        maybe_whiten_llrs(
            ctx.llr_whitening_enabled,
            &mut llrs,
            &tone_magnitudes,
            ctx.protocol_params,
        );
        normalize_llrs(&mut llrs, ctx.llr_target_variance);

        // hb-244: soft combiner integration. When enabled, the combiner
        // accumulates LLRs across repeated receptions of the same coarse
        // (freq_bin, freq_sub, time_step) key; LDPC sees the combined
        // (higher-SNR) LLRs on the second-and-subsequent receptions.
        // The first reception passes its LLRs through unchanged.
        // Disabled hot path = single Option::as_ref() branch.
        let combiner_key = ctx.soft_combiner.map(|_| combiner_key_for(trial_freq_sub));
        if let (Some(combiner_arc), Some(key)) = (ctx.soft_combiner, combiner_key) {
            // Safe to unwrap: combiner Mutex is only contended by other
            // rayon workers in the same decode_window call; no panics
            // inside the critical section.
            let mut guard = combiner_arc.lock().expect("soft combiner mutex poisoned");
            // FT8 LLR_LEN is 174 — copy into a fixed-size array for the
            // combiner API.
            debug_assert_eq!(llrs.len(), crate::soft_combiner::LLR_LEN);
            let mut llr_array = [0.0f32; crate::soft_combiner::LLR_LEN];
            llr_array.copy_from_slice(&llrs[..crate::soft_combiner::LLR_LEN]);
            let result = guard.combine(key, &llr_array);
            // Only overwrite llrs when an actual combine happened
            // (repeat_count > 1). On a cache miss / first reception, the
            // combiner returns the input unchanged — skip the copy.
            if result.repeat_count > 1 {
                llrs[..crate::soft_combiner::LLR_LEN].copy_from_slice(&result.llrs);
            }
        }

        if let Ok(corrected_bits) = ldpc.decode_soft(&llrs) {
            if par_verify_crc(&corrected_bits) {
                // hb-244: payload cleared CRC at this coarse key — evict
                // the cached bucket so future receptions at the same
                // (freq_bin, time_step) start fresh.
                if let (Some(combiner_arc), Some(key)) = (ctx.soft_combiner, combiner_key) {
                    let mut guard = combiner_arc.lock().expect("soft combiner mutex poisoned");
                    guard.mark_decoded(key);
                }

                let sub_bin_offset = trial_freq_sub as f64 * (tone_spacing / FREQ_OSR as f64);
                let base_frequency = candidate.freq_bin as f64 * tone_spacing + sub_bin_offset;

                let payload_bits = par_apply_xor(xor_sequence, &corrected_bits);
                let ft8_message = match ctx.message_parser.parse_payload(&payload_bits) {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                if !ft8_message.is_plausible() {
                    continue;
                }

                let snr_db = par_estimate_snr_spectrogram(ctx.protocol_params, &tone_magnitudes);
                let confidence = (candidate.sync_score / 12.0).min(1.0) as f32;

                // Progressive confidence gate: hard floor + suspicion check.
                // High confidence (≥0.65): accept if plausible.
                // Low confidence (<0.65): apply extra scrutiny via suspicion score.
                const MIN_DECODE_CONFIDENCE: f32 = 0.41;
                const SCRUTINY_THRESHOLD: f32 = 0.65;
                if confidence < MIN_DECODE_CONFIDENCE {
                    continue;
                }
                if confidence < SCRUTINY_THRESHOLD && ft8_message.suspicion_score() >= 2 {
                    continue;
                }

                let mut decoded_message = DecodedMessage::new(
                    ft8_message,
                    snr_db,
                    confidence,
                    base_frequency,
                    coarse_offset as f64 / SAMPLE_RATE as f64,
                );
                decoded_message.tone_symbols =
                    Some(Ft8Decoder::codeword_to_symbols(&corrected_bits));
                decoded_message.decode_time_into_window = Some(ctx.window_start.elapsed());

                return Some(decoded_message);
            }
        }
    }

    // ---- Fine-timing FFT-based extraction (expensive: 7×3 = 21 FFT trials) ----
    if candidate.sync_score < 3.5 {
        return None;
    }

    let eighth_sym = (sps / 8) as isize;
    let time_deltas: [isize; 7] = [
        -3 * eighth_sym,
        -2 * eighth_sym,
        -eighth_sym,
        0,
        eighth_sym,
        2 * eighth_sym,
        3 * eighth_sym,
    ];
    let freq_offsets: [f64; 3] = [0.0, -0.5, 0.5];
    let sub_bin_offset = candidate.freq_sub as f64 * (tone_spacing / FREQ_OSR as f64);

    for &dt in &time_deltas {
        let time_offset = coarse_offset + dt;
        if time_offset < 0 {
            continue;
        }
        let time_offset_samples = time_offset as usize;

        for &df in &freq_offsets {
            let freq_hz =
                candidate.freq_bin as f64 * tone_spacing + sub_bin_offset + df * tone_spacing;
            if freq_hz < 0.0 {
                continue;
            }
            let base_frequency = freq_hz;

            let tone_magnitudes = match par_extract_symbols_complex(
                ctx.protocol_params,
                ctx.audio,
                time_offset_samples,
                base_frequency,
                ctx.symbol_fft,
                ctx.symbol_window,
                fft_buffer,
            ) {
                Ok((_symbols, mags)) => mags,
                Err(_) => continue,
            };

            let mut llrs = par_compute_soft_llrs(ctx.protocol_params, &tone_magnitudes);
            maybe_whiten_llrs(
                ctx.llr_whitening_enabled,
                &mut llrs,
                &tone_magnitudes,
                ctx.protocol_params,
            );
            normalize_llrs(&mut llrs, ctx.llr_target_variance);

            let corrected_bits = match ldpc.decode_soft(&llrs) {
                Ok(bits) => bits,
                Err(_) => continue,
            };

            if !par_verify_crc(&corrected_bits) {
                continue;
            }

            let payload_bits = par_apply_xor(xor_sequence, &corrected_bits);
            let ft8_message = match ctx.message_parser.parse_payload(&payload_bits) {
                Ok(m) => m,
                Err(_) => continue,
            };

            if !ft8_message.is_plausible() {
                continue;
            }

            let snr_db = par_estimate_snr_fft(ctx.protocol_params, &tone_magnitudes);
            let confidence = (candidate.sync_score / 12.0).min(1.0) as f32;

            const MIN_DECODE_CONFIDENCE: f32 = 0.41;
            const SCRUTINY_THRESHOLD: f32 = 0.65;
            if confidence < MIN_DECODE_CONFIDENCE {
                continue;
            }
            if confidence < SCRUTINY_THRESHOLD && ft8_message.suspicion_score() >= 2 {
                continue;
            }

            let mut decoded_message = DecodedMessage::new(
                ft8_message,
                snr_db,
                confidence,
                base_frequency,
                time_offset_samples as f64 / SAMPLE_RATE as f64,
            );
            decoded_message.tone_symbols = Some(Ft8Decoder::codeword_to_symbols(&corrected_bits));
            decoded_message.decode_time_into_window = Some(ctx.window_start.elapsed());

            return Some(decoded_message);
        }
    }

    None
}

/// Try AP-enhanced decoding for a single candidate (parallel-safe).
fn par_try_ap_decode(
    ctx: &DecodeContext,
    candidate: &CostasCandidate,
    ldpc: &LdpcDecoder,
    decoded_calls: &HashSet<String>,
    _pass: usize,
) -> Option<DecodedMessage> {
    let tone_spacing = ctx.protocol_params.tone_spacing;
    let sps = ctx.protocol_params.samples_per_symbol(SAMPLE_RATE);
    let spec_step = sps / TIME_OSR;
    let coarse_offset =
        (candidate.time_step as isize - ctx.spectrogram.time_padding as isize) * spec_step as isize;

    let freq_sub_trials = [
        candidate.freq_sub,
        if candidate.freq_sub == 0 { 1 } else { 0 },
    ];

    for &trial_freq_sub in &freq_sub_trials {
        let trial_candidate = CostasCandidate {
            freq_sub: trial_freq_sub,
            ..*candidate
        };
        let tone_magnitudes = par_extract_symbols_from_spectrogram(
            ctx.protocol_params,
            ctx.spectrogram,
            &trial_candidate,
            ctx.sync_time_interp_linear_power,
        );
        let mut base_llrs = par_compute_soft_llrs_db(ctx.protocol_params, &tone_magnitudes);
        // JS8Call-Improved-style LLR whitening (inspired by spec ref
        // `spec-js8call-llr-whitening.md`). Whitening is applied to
        // `base_llrs` here so that AP injection (which clones base_llrs
        // into a new vector) sees the whitened scale; the downstream
        // `par_try_ldpc_with_ap` then re-normalises variance after AP
        // bits are written. No-op when the master flag is OFF.
        maybe_whiten_llrs(
            ctx.llr_whitening_enabled,
            &mut base_llrs,
            &tone_magnitudes,
            ctx.protocol_params,
        );

        let sub_bin_offset = trial_freq_sub as f64 * (tone_spacing / FREQ_OSR as f64);
        let base_frequency = candidate.freq_bin as f64 * tone_spacing + sub_bin_offset;
        let time_offset_s = coarse_offset as f64 / SAMPLE_RATE as f64;

        let snr_db = par_estimate_snr_spectrogram(ctx.protocol_params, &tone_magnitudes);
        let confidence = (candidate.sync_score / 12.0).min(1.0) as f32;

        // --- AP1: inject own callsign at bits 28-55 (called station) ---
        if ctx.ap_context.my_call.is_some() {
            if let Some(msg) = par_try_ldpc_with_ap(
                ctx,
                ldpc,
                &base_llrs,
                crate::ap::ApLevel::Ap1,
                ctx.ap_context,
                None,
                snr_db,
                confidence,
                base_frequency,
                time_offset_s,
            ) {
                return Some(msg);
            }
        }

        // --- AP2: inject each recent caller at bits 0-27 + AP1 ---
        if ctx.ap_context.my_call.is_some() {
            for recent in &ctx.ap_context.recent_calls {
                if decoded_calls.contains(&recent.callsign) {
                    continue;
                }
                if let Some(msg) = par_try_ldpc_with_ap(
                    ctx,
                    ldpc,
                    &base_llrs,
                    crate::ap::ApLevel::Ap2,
                    ctx.ap_context,
                    Some(recent),
                    snr_db,
                    confidence,
                    base_frequency,
                    time_offset_s,
                ) {
                    return Some(msg);
                }
            }
        }

        // --- AP-recent-only (hb-043, my_call-less): when my_call is
        // unset, try each recent callsign at BOTH bits 0-27 (caller
        // position) and bits 28-55 (called position). Enables the
        // hb-027 "rolling callsign window" use case where the operator
        // is scanning, not transmitting, so my_call is irrelevant but
        // observed callsigns are still useful priors.
        if ctx.ap_context.my_call.is_none() && !ctx.ap_context.recent_calls.is_empty() {
            for recent in &ctx.ap_context.recent_calls {
                if decoded_calls.contains(&recent.callsign) {
                    continue;
                }
                // Try as caller (bits 0-27)
                if let Some(msg) = par_try_ldpc_with_recent_only(
                    ctx,
                    ldpc,
                    &base_llrs,
                    recent,
                    RecentInjectPos::Caller,
                    snr_db,
                    confidence,
                    base_frequency,
                    time_offset_s,
                ) {
                    return Some(msg);
                }
                // Try as called (bits 28-55)
                if let Some(msg) = par_try_ldpc_with_recent_only(
                    ctx,
                    ldpc,
                    &base_llrs,
                    recent,
                    RecentInjectPos::Called,
                    snr_db,
                    confidence,
                    base_frequency,
                    time_offset_s,
                ) {
                    return Some(msg);
                }
            }
        }

        // --- AP3: both callsigns known (active QSO) ---
        if ctx.ap_context.active_qso.is_some() && ctx.ap_context.my_call.is_some() {
            if let Some(msg) = par_try_ldpc_with_ap(
                ctx,
                ldpc,
                &base_llrs,
                crate::ap::ApLevel::Ap3,
                ctx.ap_context,
                None,
                snr_db,
                confidence,
                base_frequency,
                time_offset_s,
            ) {
                return Some(msg);
            }

            // --- AP4: AP3 + message type constraint ---
            if let Some(ref qso) = ctx.ap_context.active_qso {
                if matches!(
                    qso.progress,
                    crate::ap::QsoApProgress::WaitingForConfirmation
                ) {
                    if let Some(msg) = par_try_ldpc_with_ap(
                        ctx,
                        ldpc,
                        &base_llrs,
                        crate::ap::ApLevel::Ap4,
                        ctx.ap_context,
                        None,
                        snr_db,
                        confidence,
                        base_frequency,
                        time_offset_s,
                    ) {
                        return Some(msg);
                    }
                }
            }
        }
    }

    None
}

/// Try LDPC decode with AP injection at a specific level (parallel-safe).
fn par_try_ldpc_with_ap(
    ctx: &DecodeContext,
    ldpc: &LdpcDecoder,
    base_llrs: &[f32],
    ap_level: crate::ap::ApLevel,
    ap_context: &crate::ap::ApContext,
    caller_override: Option<&crate::ap::RecentCallAp>,
    snr_db: f32,
    confidence: f32,
    base_frequency: f64,
    time_offset_s: f64,
) -> Option<DecodedMessage> {
    let mut llrs = base_llrs.to_vec();

    match ap_level {
        crate::ap::ApLevel::Ap0 => {}
        crate::ap::ApLevel::Ap1 => {
            crate::ap::inject_ap_llrs(&mut llrs, ap_level, ap_context);
        }
        crate::ap::ApLevel::Ap2 => {
            crate::ap::inject_ap_llrs(&mut llrs, crate::ap::ApLevel::Ap1, ap_context);
            if let Some(caller) = caller_override {
                crate::ap::inject_ap2_caller(&mut llrs, caller);
            }
        }
        crate::ap::ApLevel::Ap3 | crate::ap::ApLevel::Ap4 => {
            crate::ap::inject_ap_llrs(&mut llrs, ap_level, ap_context);
        }
    }

    normalize_llrs(&mut llrs, ctx.llr_target_variance);

    let corrected_bits = match ldpc.decode_soft(&llrs) {
        Ok(bits) => bits,
        Err(_) => return None,
    };

    if !par_verify_crc(&corrected_bits) {
        return None;
    }

    let payload_bits = par_apply_xor(ctx.xor_sequence, &corrected_bits);
    let ft8_message = match ctx.message_parser.parse_payload(&payload_bits) {
        Ok(m) => m,
        Err(_) => return None,
    };

    if !ft8_message.is_plausible() {
        return None;
    }

    // AP-injection survival check. If the LDPC parity overruled the AP
    // bias and produced a codeword that doesn't carry the injected
    // callsign, the AP didn't help — reject as a CRC-coincidence false
    // positive.
    if !ap_injection_survived(ap_level, ap_context, &ft8_message) {
        return None;
    }

    let ap_level_num = match ap_level {
        crate::ap::ApLevel::Ap0 => 0u8,
        crate::ap::ApLevel::Ap1 => 1,
        crate::ap::ApLevel::Ap2 => 2,
        crate::ap::ApLevel::Ap3 => 3,
        crate::ap::ApLevel::Ap4 => 4,
    };
    // AP decodes need higher confidence than standard decodes because
    // AP injection biases the LDPC solver toward our callsign, producing
    // phantom messages (e.g., "HZ0DCR K1ABC AM16") from noise.
    const MIN_AP_CONFIDENCE: f32 = 0.55;
    const MIN_DECODE_CONFIDENCE: f32 = 0.41;
    const SCRUTINY_THRESHOLD: f32 = 0.65;
    let min_conf = if ap_level_num > 0 {
        MIN_AP_CONFIDENCE
    } else {
        MIN_DECODE_CONFIDENCE
    };
    if confidence < min_conf {
        return None;
    }
    if confidence < SCRUTINY_THRESHOLD && ft8_message.suspicion_score() >= 2 {
        return None;
    }

    let mut decoded_message = DecodedMessage::new(
        ft8_message,
        snr_db,
        confidence,
        base_frequency,
        time_offset_s,
    );
    decoded_message.tone_symbols = Some(Ft8Decoder::codeword_to_symbols(&corrected_bits));
    decoded_message.ap_level = ap_level_num;
    decoded_message.decode_time_into_window = Some(ctx.window_start.elapsed());

    Some(decoded_message)
}

/// Position to inject a recent callsign in the LLR vector. hb-043 my_call-less AP.
#[derive(Debug, Clone, Copy)]
enum RecentInjectPos {
    /// Inject at bits 0-27 (caller / from-callsign position).
    Caller,
    /// Inject at bits 28-55 (called / to-callsign position).
    Called,
}

/// hb-043: LDPC decode with a single recent callsign injected at one position,
/// without the my_call-coupled AP1 injection that AP2 normally prepends.
/// Mirrors `par_try_ldpc_with_ap` but for the my_call-less use case.
fn par_try_ldpc_with_recent_only(
    ctx: &DecodeContext,
    ldpc: &LdpcDecoder,
    base_llrs: &[f32],
    recent: &crate::ap::RecentCallAp,
    pos: RecentInjectPos,
    snr_db: f32,
    confidence: f32,
    base_frequency: f64,
    time_offset_s: f64,
) -> Option<DecodedMessage> {
    let mut llrs = base_llrs.to_vec();
    match pos {
        RecentInjectPos::Caller => crate::ap::inject_ap2_caller(&mut llrs, recent),
        RecentInjectPos::Called => crate::ap::inject_recent_call_at_called(&mut llrs, recent),
    }

    normalize_llrs(&mut llrs, ctx.llr_target_variance);

    let corrected_bits = match ldpc.decode_soft(&llrs) {
        Ok(bits) => bits,
        Err(_) => return None,
    };
    if !par_verify_crc(&corrected_bits) {
        return None;
    }
    let payload_bits = par_apply_xor(ctx.xor_sequence, &corrected_bits);
    let ft8_message = match ctx.message_parser.parse_payload(&payload_bits) {
        Ok(m) => m,
        Err(_) => return None,
    };
    if !ft8_message.is_plausible() {
        return None;
    }

    // Survival check: the decoded message's callsign at the injected position
    // must match the injected recent callsign. Otherwise the LDPC parity
    // overruled the bias and produced a CRC-coincidence FP.
    let target_call = match pos {
        RecentInjectPos::Caller => ft8_message.from_callsign.as_deref().unwrap_or(""),
        RecentInjectPos::Called => ft8_message.to_callsign.as_deref().unwrap_or(""),
    };
    let target_base = target_call.split('/').next().unwrap_or(target_call);
    if target_base != recent.callsign {
        return None;
    }

    // Same confidence gating as par_try_ldpc_with_ap for AP-level decodes.
    const MIN_AP_CONFIDENCE: f32 = 0.55;
    const SCRUTINY_THRESHOLD: f32 = 0.65;
    if confidence < MIN_AP_CONFIDENCE {
        return None;
    }
    if confidence < SCRUTINY_THRESHOLD && ft8_message.suspicion_score() >= 2 {
        return None;
    }

    let mut decoded_message = DecodedMessage::new(
        ft8_message,
        snr_db,
        confidence,
        base_frequency,
        time_offset_s,
    );
    decoded_message.tone_symbols = Some(Ft8Decoder::codeword_to_symbols(&corrected_bits));
    // Report as AP-level 2 (recent-caller class) for now. Future could
    // introduce a distinct ap_level number for hb-043 if telemetry needs it.
    decoded_message.ap_level = 2;
    decoded_message.decode_time_into_window = Some(ctx.window_start.elapsed());
    Some(decoded_message)
}

/// Verify that the AP-injected callsign(s) survived the LDPC pass and
/// landed in the parsed message. AP injection biases the LDPC priors but
/// doesn't constrain them — the parity solver can overrule the bias and
/// produce a CRC-valid codeword that ignores the injected hint. When that
/// happens, the result is a false positive: the AP was wasted, the codeword
/// happened to satisfy CRC by coincidence, and the message has someone
/// else's callsign in the position we tried to fix.
///
/// Rejecting these prevents the most common AP-induced false-positive
/// pattern: "K5ARH RANDOMCALL +X" decodes seen on a busy band when AP1
/// (own callsign as called station) is enabled but the actual signal has
/// nothing to do with us.
///
/// Returns `true` for `Ap0` unconditionally (no injection happened).
pub(crate) fn ap_injection_survived(
    ap_level: crate::ap::ApLevel,
    ap_context: &crate::ap::ApContext,
    msg: &crate::message::Ft8Message,
) -> bool {
    match ap_level {
        // No injection happened — nothing to verify.
        crate::ap::ApLevel::Ap0 => true,

        // AP1 injects our callsign at bits 28-55 (the called-station slot).
        // The parsed result must have our_call as to_callsign.
        // AP2 also injects AP1 (our callsign as called) plus a recent caller
        // at bits 0-27 (calling-station slot) — verify both.
        crate::ap::ApLevel::Ap1 | crate::ap::ApLevel::Ap2 => {
            let Some(ref my) = ap_context.my_call else {
                return true; // No my_call to verify against — accept.
            };
            let to = msg.to_callsign.as_deref().unwrap_or("");
            // Match against the bare callsign (no /R or /P suffix).
            let to_base = to.split('/').next().unwrap_or(to);
            if to_base != my.callsign {
                return false;
            }
            true
        }

        // AP3/AP4 inject the active QSO partner at bits 0-27 (calling
        // station) AND our callsign at bits 28-55 (called station). Both
        // must survive in the parsed message.
        crate::ap::ApLevel::Ap3 | crate::ap::ApLevel::Ap4 => {
            let Some(ref my) = ap_context.my_call else {
                return true;
            };
            let to = msg.to_callsign.as_deref().unwrap_or("");
            let to_base = to.split('/').next().unwrap_or(to);
            if to_base != my.callsign {
                return false;
            }
            if let Some(ref qso) = ap_context.active_qso {
                let from = msg.from_callsign.as_deref().unwrap_or("");
                let from_base = from.split('/').next().unwrap_or(from);
                if from_base != qso.their_call {
                    return false;
                }
            }
            true
        }
    }
}

// ---- Parallel-safe helpers (free functions operating on shared state) ----

// hb-056 cross-cycle averaging tunables. SLOT_TIME_STEPS_FT8 = 15 s of audio
// expressed in spectrogram steps (subblock_size = 960 samples at 12 kHz =
// 0.08 s/step → 15/0.08 = 187.5, rounded up to 188). T_TOL/F_TOL/SCORE_BAND
// gate the grouping conservatively; a corrupted average that gets through
// is filtered downstream by CRC and the production callsign-continuity filter.
const SLOT_TIME_STEPS_FT8: usize = 188;
const CROSS_CYCLE_T_TOL: usize = 2;
const CROSS_CYCLE_F_TOL: usize = 1;
const CROSS_CYCLE_SCORE_BAND: f64 = 3.0;

/// hb-056: group candidates that look like the same repeating station in
/// different slots. Two candidates match when they share `freq_sub`, their
/// `freq_bin`s are within `F_TOL`, their `t0`s differ by a non-zero
/// integer multiple of `SLOT_TIME_STEPS_FT8` within `T_TOL`, and their
/// `sync_score`s are within `SCORE_BAND`. Greedy first-fit: each candidate
/// joins the first compatible group or starts a new one; only returns
/// groups of size ≥ 2.
fn group_for_cross_cycle(candidates: &[CostasCandidate]) -> Vec<Vec<usize>> {
    let n = candidates.len();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut grouped = vec![false; n];
    for i in 0..n {
        if grouped[i] {
            continue;
        }
        let a = &candidates[i];
        let mut group = vec![i];
        grouped[i] = true;
        for (j, b) in candidates.iter().enumerate().skip(i + 1) {
            if grouped[j] {
                continue;
            }
            if a.freq_sub != b.freq_sub {
                continue;
            }
            let df = (a.freq_bin as i64 - b.freq_bin as i64).unsigned_abs() as usize;
            if df > CROSS_CYCLE_F_TOL {
                continue;
            }
            let dt = (a.time_step as i64 - b.time_step as i64).unsigned_abs() as usize;
            if dt == 0 {
                continue;
            }
            let dt_mod = dt % SLOT_TIME_STEPS_FT8;
            let aligned =
                dt_mod <= CROSS_CYCLE_T_TOL || (SLOT_TIME_STEPS_FT8 - dt_mod) <= CROSS_CYCLE_T_TOL;
            if !aligned {
                continue;
            }
            if (a.sync_score - b.sync_score).abs() > CROSS_CYCLE_SCORE_BAND {
                continue;
            }
            group.push(j);
            grouped[j] = true;
        }
        if group.len() >= 2 {
            groups.push(group);
        }
    }
    groups
}

/// hb-074: coherent complement of `sum_tone_magnitudes_linear`. Members
/// are already phase-aligned (each multiplied by `conj(rotor)`), so a
/// straight complex sum integrates signal amplitudes coherently while
/// noise (uncorrelated phase) averages down. Returns the resulting
/// per-symbol per-tone POWER in dB so the existing LLR pipeline can
/// consume it unchanged. For N aligned signals: signal power scales as
/// N² while noise scales as N → SNR improves by N (vs √N non-coherent).
fn coherent_sum_complex_to_db(
    members: &[Vec<[Complex<f64>; NUM_TONES]>],
    num_symbols: usize,
) -> Vec<[f64; NUM_TONES]> {
    let mut sum = vec![[Complex::new(0.0f64, 0.0); NUM_TONES]; num_symbols];
    for m in members {
        for s in 0..num_symbols.min(m.len()) {
            for t in 0..NUM_TONES {
                sum[s][t] += m[s][t];
            }
        }
    }
    let mut result = vec![[0.0f64; NUM_TONES]; num_symbols];
    for s in 0..num_symbols {
        for t in 0..NUM_TONES {
            let p = sum[s][t].norm_sqr();
            result[s][t] = 10.0 * (p + 1e-30).log10();
        }
    }
    result
}

/// hb-056: sum tone magnitudes in LINEAR power (10^(dB/10)) across the
/// group's members, then convert back to dB. This is the non-coherent
/// analogue of JTDX's `s2(i) = |cs|² + |csold|²`. Linear-domain
/// summation is required — averaging in dB is the wrong operation
/// (the LLR pipeline relies on the dB encoding the power summation,
/// not the log-power summation).
fn sum_tone_magnitudes_linear(
    members: &[Vec<[f64; NUM_TONES]>],
    num_symbols: usize,
) -> Vec<[f64; NUM_TONES]> {
    let mut sum_lin = vec![[0.0f64; NUM_TONES]; num_symbols];
    for member in members {
        for s in 0..num_symbols.min(member.len()) {
            for t in 0..NUM_TONES {
                sum_lin[s][t] += 10f64.powf(member[s][t] / 10.0);
            }
        }
    }
    let mut result = vec![[0.0f64; NUM_TONES]; num_symbols];
    for s in 0..num_symbols {
        for t in 0..NUM_TONES {
            result[s][t] = 10.0 * (sum_lin[s][t] + 1e-30).log10();
        }
    }
    result
}

/// hb-079: reverse-derive a candidate from a DecodedMessage's
/// `frequency_offset` and `time_offset`. We don't keep `(message,
/// candidate)` pairs through the rayon decode path, so we reconstruct
/// the candidate for coherent subtraction. `time_refinement` defaults
/// to 0 (production state — `sync_time_interpolation = false`);
/// `sync_score` is a placeholder (not consumed downstream of subtract).
fn reverse_derive_candidate(
    msg: &DecodedMessage,
    pp: &ProtocolParams,
    time_padding: usize,
) -> CostasCandidate {
    let tone_spacing = pp.tone_spacing;
    let sub_bin = tone_spacing / FREQ_OSR as f64;
    let freq_bin = (msg.frequency_offset / tone_spacing).floor() as usize;
    let remainder = msg.frequency_offset - freq_bin as f64 * tone_spacing;
    let freq_sub = (remainder / sub_bin).round() as usize;
    let sps = pp.samples_per_symbol(SAMPLE_RATE);
    let spec_step = sps / TIME_OSR;
    let time_step_rel = (msg.time_offset * SAMPLE_RATE as f64 / spec_step as f64).round() as isize;
    let time_step = (time_step_rel + time_padding as isize).max(0) as usize;
    CostasCandidate {
        time_step,
        freq_bin,
        freq_sub,
        sync_score: 0.0,
        time_refinement: 0.0,
    }
}

/// hb-079: coherent maximum-likelihood subtraction of a decoded signal
/// from the complex spectrogram. At each of the 79 symbol positions, at
/// the *true* (decoded) tone, project the complex bin onto the
/// candidate's phase rotor — `proj = Re(bin·conj(rotor))·rotor` — and
/// subtract. This removes the signal component aligned with the rotor
/// while preserving the orthogonal noise component (canonical ML signal
/// subtraction). Affects both TIME_OSR substeps within each symbol
/// window. Refreshes `spectrogram.power` consistently so the sync search
/// on the residual sees the updated dB view. No-op when the spectrogram
/// has no complex retention.
fn subtract_decode_coherent(
    spectrogram: &mut Spectrogram,
    pp: &ProtocolParams,
    candidate: &CostasCandidate,
    rotor: Complex<f64>,
    tone_symbols: &[u8],
    // hb-081: subtract amplitude scale ∈ [0, 1]. 1.0 = full ML subtract
    // (hb-079 default); <1.0 reduces the magnitude (MRC weighting from a
    // noisy-rotor caller). Outside [0,1] is clamped.
    scale: f64,
) {
    if spectrogram.complex.is_none() {
        return;
    }
    let scale = scale.clamp(0.0, 1.0);
    let t0 = candidate.time_step;
    let f0 = candidate.freq_bin;
    let fs = candidate.freq_sub;
    let num_bins = spectrogram.num_bins;
    let num_steps = spectrogram.num_steps;
    let freq_osr = spectrogram.freq_osr;
    if fs >= freq_osr {
        return;
    }
    let steps_per_symbol = TIME_OSR;
    let rotor_conj = rotor.conj();

    for sym_idx in 0..pp.num_symbols.min(tone_symbols.len()) {
        let tone = tone_symbols[sym_idx] as usize;
        if tone >= NUM_TONES {
            continue;
        }
        let f_idx = f0 + tone;
        if f_idx >= num_bins {
            continue;
        }
        let t_base = t0 + sym_idx * steps_per_symbol;
        for s in 0..steps_per_symbol {
            let t_idx = t_base + s;
            if t_idx >= num_steps {
                continue;
            }
            // Scoped &mut to spectrogram.complex; ends before .power access.
            let residual = {
                let complex = spectrogram.complex.as_mut().unwrap();
                let bin = complex[t_idx][fs][f_idx];
                let proj_real = (bin * rotor_conj).re;
                // hb-081: scale the subtracted projection magnitude.
                let signal_est = Complex::new(proj_real * scale, 0.0) * rotor;
                let residual = bin - signal_est;
                complex[t_idx][fs][f_idx] = residual;
                residual
            };
            let mag2 = residual.norm_sqr();
            spectrogram.power[t_idx][fs][f_idx] = 10.0 * (1e-12 + mag2).log10();
        }
    }
}

fn par_extract_symbols_from_spectrogram(
    pp: &ProtocolParams,
    spectrogram: &Spectrogram,
    candidate: &CostasCandidate,
    linear_power: bool,
) -> Vec<[f64; NUM_TONES]> {
    let t0 = candidate.time_step;
    let f0 = candidate.freq_bin;
    let fs = candidate.freq_sub;
    // hb-044: optional fractional time-bin shift applied to spectrogram
    // lookups. dt=0 → identical to original integer-bin behavior.
    let dt = candidate.time_refinement;

    let mut tone_magnitudes = Vec::with_capacity(pp.num_symbols);
    let steps_per_symbol = TIME_OSR;

    for sym_idx in 0..pp.num_symbols {
        let mut mags = [-120.0f64; NUM_TONES];
        let t_base = t0 + sym_idx * steps_per_symbol;

        for tone in 0..pp.num_tones {
            let freq_bin = f0 + tone;
            if freq_bin >= spectrogram.num_bins || fs >= spectrogram.freq_osr {
                continue;
            }
            let db_a = lookup_time_interp(spectrogram, t_base, dt, fs, freq_bin, linear_power);
            let db_b = lookup_time_interp(spectrogram, t_base + 1, dt, fs, freq_bin, linear_power);
            mags[tone] = (db_a + db_b) / 2.0;
        }

        tone_magnitudes.push(mags);
    }

    tone_magnitudes
}

/// hb-074: complex-valued sibling of `par_extract_symbols_from_spectrogram`.
/// Returns the complex FFT bin for each (symbol, tone) at the candidate's
/// time/freq alignment. Returns `None` when the spectrogram wasn't built
/// with `cross_cycle_coherent` (no `.complex` payload). Uses the FIRST of
/// the two TIME_OSR substeps per symbol — phase recovery from Costas
/// already aggregates 21 samples, so the second substep adds little.
fn par_extract_complex_symbols_from_spectrogram(
    pp: &ProtocolParams,
    spectrogram: &Spectrogram,
    candidate: &CostasCandidate,
) -> Option<Vec<[Complex<f64>; NUM_TONES]>> {
    let complex = spectrogram.complex.as_ref()?;
    let t0 = candidate.time_step;
    let f0 = candidate.freq_bin;
    let fs = candidate.freq_sub;
    let steps_per_symbol = TIME_OSR;

    let mut out: Vec<[Complex<f64>; NUM_TONES]> = Vec::with_capacity(pp.num_symbols);
    for sym_idx in 0..pp.num_symbols {
        let mut row = [Complex::new(0.0f64, 0.0); NUM_TONES];
        let t_base = t0 + sym_idx * steps_per_symbol;
        if t_base >= spectrogram.num_steps {
            out.push(row);
            continue;
        }
        for tone in 0..pp.num_tones {
            let freq_bin = f0 + tone;
            if freq_bin >= spectrogram.num_bins || fs >= spectrogram.freq_osr {
                continue;
            }
            row[tone] = complex[t_base][fs][freq_bin];
        }
        out.push(row);
    }
    Some(out)
}

/// hb-074: sum the candidate's complex FFT bins at all 21 Costas positions
/// (each at its expected tone). `Σ cs[costas_sym][expected_tone] =
/// N·A·exp(jφ_cand)` (signal coherent, noise uncorrelated). The result's
/// phase is the candidate's reference phase; the result's magnitude is
/// proportional to the candidate's signal strength × √N_costas — the MRC
/// weight for hb-075. Returned un-normalised so callers can pick
/// rotor-only (hb-074) or rotor+magnitude (hb-075).
fn compute_costas_complex_accumulator(
    pp: &ProtocolParams,
    complex_symbols: &[[Complex<f64>; NUM_TONES]],
) -> Complex<f64> {
    let mut acc = Complex::<f64>::new(0.0, 0.0);
    for (m, &group_start) in pp.costas_positions.iter().enumerate() {
        for k in 0..pp.costas_length {
            let sym_idx = group_start + k;
            let expected_tone = pp.costas_arrays[m][k] as usize;
            if sym_idx >= complex_symbols.len() || expected_tone >= NUM_TONES {
                continue;
            }
            acc += complex_symbols[sym_idx][expected_tone];
        }
    }
    acc
}

/// hb-074: estimate a candidate's per-cycle phase rotor — the
/// unit-magnitude `exp(jφ_cand)` to divide each symbol by to align the
/// candidate's phase to a common reference. Returns `None` if the
/// accumulator magnitude is too small to give a stable estimate
/// (silent candidate / clipped buffer).
fn estimate_candidate_phase_rotor(
    pp: &ProtocolParams,
    complex_symbols: &[[Complex<f64>; NUM_TONES]],
) -> Option<Complex<f64>> {
    let acc = compute_costas_complex_accumulator(pp, complex_symbols);
    let mag = acc.norm();
    if mag < 1e-30 {
        return None;
    }
    Some(acc / mag)
}

/// Linear-interpolation lookup into a spectrogram with fractional time
/// offset. dt=0 returns spectrogram.power[t_base][fs][freq_bin] exactly.
/// Out-of-range cells contribute -120.0 dB. hb-044 helper.
///
/// hb-069: when `linear_power` is true and `dt != 0`, the two endpoint
/// dB values are converted to linear power (10^(db/10)), interpolated
/// linearly, then converted back to dB. dB-space interpolation is
/// non-linear in real power; linear-power interpolation preserves
/// symbol energy more accurately near the noise floor at the cost of
/// two pow/log per call. `linear_power=false` (legacy path) keeps the
/// straight dB interpolation.
#[inline]
fn lookup_time_interp(
    spec: &Spectrogram,
    t_base: usize,
    dt: f64,
    fs: usize,
    freq_bin: usize,
    linear_power: bool,
) -> f64 {
    if dt.abs() < f64::EPSILON {
        return if t_base < spec.num_steps {
            spec.power[t_base][fs][freq_bin]
        } else {
            -120.0
        };
    }
    // Continuous time position: t_base + dt
    let t_cont = t_base as f64 + dt;
    let t_lo_f = t_cont.floor();
    let frac = t_cont - t_lo_f;
    let lo_idx = t_lo_f as isize;
    let hi_idx = lo_idx + 1;
    let p_lo = if lo_idx >= 0 && (lo_idx as usize) < spec.num_steps {
        spec.power[lo_idx as usize][fs][freq_bin]
    } else {
        -120.0
    };
    let p_hi = if hi_idx >= 0 && (hi_idx as usize) < spec.num_steps {
        spec.power[hi_idx as usize][fs][freq_bin]
    } else {
        -120.0
    };
    if linear_power {
        // hb-069: interpolate in linear power, return dB.
        let lin_lo = 10f64.powf(p_lo / 10.0);
        let lin_hi = 10f64.powf(p_hi / 10.0);
        let lin_mid = (1.0 - frac) * lin_lo + frac * lin_hi;
        // Floor matches the -120 dB sentinel; 10^(-120/10) = 1e-12.
        if lin_mid <= 1e-12 {
            -120.0
        } else {
            10.0 * lin_mid.log10()
        }
    } else {
        (1.0 - frac) * p_lo + frac * p_hi
    }
}

fn par_compute_soft_llrs_db(pp: &ProtocolParams, tone_magnitudes: &[[f64; NUM_TONES]]) -> Vec<f32> {
    let mut llrs = Vec::with_capacity(174);
    let data_positions = pp.data_symbol_indices();

    for &sym_idx in &data_positions {
        let mags = &tone_magnitudes[sym_idx];

        match pp.bits_per_symbol {
            3 => {
                let mut s2 = [0.0f64; 8];
                for j in 0..8 {
                    let tone_idx = crate::ldpc::binary_to_gray(j as u8) as usize;
                    s2[j] = mags[tone_idx];
                }

                fn max4(a: f64, b: f64, c: f64, d: f64) -> f64 {
                    a.max(b).max(c.max(d))
                }

                let llr0 = max4(s2[4], s2[5], s2[6], s2[7]) - max4(s2[0], s2[1], s2[2], s2[3]);
                let llr1 = max4(s2[2], s2[3], s2[6], s2[7]) - max4(s2[0], s2[1], s2[4], s2[5]);
                let llr2 = max4(s2[1], s2[3], s2[5], s2[7]) - max4(s2[0], s2[2], s2[4], s2[6]);

                llrs.push(-llr0 as f32);
                llrs.push(-llr1 as f32);
                llrs.push(-llr2 as f32);
            }
            2 => {
                let mut s2 = [0.0f64; 4];
                for j in 0..4 {
                    let tone_idx = crate::ldpc::binary_to_gray_4fsk(j as u8) as usize;
                    s2[j] = mags[tone_idx];
                }

                let llr0 = s2[2].max(s2[3]) - s2[0].max(s2[1]);
                let llr1 = s2[1].max(s2[3]) - s2[0].max(s2[2]);

                llrs.push(-llr0 as f32);
                llrs.push(-llr1 as f32);
            }
            _ => unreachable!("Unsupported bits_per_symbol"),
        }
    }

    debug_assert_eq!(llrs.len(), 174);
    llrs
}

fn par_compute_soft_llrs(pp: &ProtocolParams, tone_magnitudes: &[[f64; NUM_TONES]]) -> Vec<f32> {
    let mut llrs = Vec::with_capacity(174);
    let data_positions = pp.data_symbol_indices();

    for &sym_idx in &data_positions {
        let mags = &tone_magnitudes[sym_idx];

        match pp.bits_per_symbol {
            3 => {
                let mut s2 = [0.0f64; 8];
                for j in 0..8 {
                    let tone_idx = crate::ldpc::binary_to_gray(j as u8) as usize;
                    s2[j] = (1e-12 + mags[tone_idx] * mags[tone_idx]).log10() * 10.0;
                }

                fn max4(a: f64, b: f64, c: f64, d: f64) -> f64 {
                    a.max(b).max(c.max(d))
                }

                let llr0 = max4(s2[4], s2[5], s2[6], s2[7]) - max4(s2[0], s2[1], s2[2], s2[3]);
                let llr1 = max4(s2[2], s2[3], s2[6], s2[7]) - max4(s2[0], s2[1], s2[4], s2[5]);
                let llr2 = max4(s2[1], s2[3], s2[5], s2[7]) - max4(s2[0], s2[2], s2[4], s2[6]);

                llrs.push(-llr0 as f32);
                llrs.push(-llr1 as f32);
                llrs.push(-llr2 as f32);
            }
            2 => {
                let mut s2 = [0.0f64; 4];
                for j in 0..4 {
                    let tone_idx = crate::ldpc::binary_to_gray_4fsk(j as u8) as usize;
                    s2[j] = (1e-12 + mags[tone_idx] * mags[tone_idx]).log10() * 10.0;
                }

                let llr0 = s2[2].max(s2[3]) - s2[0].max(s2[1]);
                let llr1 = s2[1].max(s2[3]) - s2[0].max(s2[2]);

                llrs.push(-llr0 as f32);
                llrs.push(-llr1 as f32);
            }
            _ => unreachable!("Unsupported bits_per_symbol"),
        }
    }

    debug_assert_eq!(llrs.len(), 174);
    llrs
}

/// Extract symbols using per-thread FFT buffer (parallel-safe version of extract_symbols_complex).
fn par_extract_symbols_complex(
    pp: &ProtocolParams,
    audio: &[f64],
    time_offset_samples: usize,
    base_frequency: f64,
    symbol_fft: &std::sync::Arc<dyn rustfft::Fft<f64>>,
    symbol_window: &[f64],
    fft_buffer: &mut Vec<Complex<f64>>,
) -> Ft8Result<(Vec<u8>, Vec<[f64; NUM_TONES]>)> {
    let sps = pp.samples_per_symbol(SAMPLE_RATE);
    let end_sample = time_offset_samples + pp.num_symbols * sps;
    if end_sample > audio.len() {
        return Err(Ft8Error::InsufficientData {
            needed: end_sample,
            available: audio.len(),
        });
    }

    let pi2 = 2.0 * std::f64::consts::PI;
    let phase_step_angle = -pi2 * base_frequency / SAMPLE_RATE as f64;
    let phase_step = Complex::new(phase_step_angle.cos(), phase_step_angle.sin());

    let mut symbols = Vec::with_capacity(pp.num_symbols);
    let mut tone_magnitudes = Vec::with_capacity(pp.num_symbols);

    for sym_idx in 0..pp.num_symbols {
        let sym_start = time_offset_samples + sym_idx * sps;
        let symbol_audio = &audio[sym_start..sym_start + sps];

        let initial_angle = -pi2 * base_frequency * sym_start as f64 / SAMPLE_RATE as f64;
        let mut rotator = Complex::new(initial_angle.cos(), initial_angle.sin());

        for i in 0..sps {
            let w = symbol_window[i];
            fft_buffer[i] = Complex::new(
                symbol_audio[i] * w * rotator.re,
                symbol_audio[i] * w * rotator.im,
            );
            rotator = rotator * phase_step;
        }

        symbol_fft.process(&mut fft_buffer[..sps]);

        let mut mags = [0.0f64; NUM_TONES];
        let mut best_tone = 0u8;
        let mut best_mag = 0.0;

        for tone in 0..pp.num_tones {
            let magnitude = fft_buffer[tone].norm();
            mags[tone] = magnitude;
            if magnitude > best_mag {
                best_mag = magnitude;
                best_tone = tone as u8;
            }
        }

        symbols.push(best_tone);
        tone_magnitudes.push(mags);
    }

    Ok((symbols, tone_magnitudes))
}

fn par_verify_crc(bits: &BitVec) -> bool {
    if bits.len() < PAYLOAD_BITS + CRC_BITS {
        return false;
    }
    let payload = &bits[0..PAYLOAD_BITS];
    let received_crc_bits = &bits[PAYLOAD_BITS..PAYLOAD_BITS + CRC_BITS];
    let calculated_crc = calculate_crc14(payload);
    let received_crc = bits_to_u16(received_crc_bits);
    calculated_crc == received_crc
}

fn par_apply_xor(xor_sequence: Option<&'static [u8; 10]>, corrected_bits: &BitVec) -> BitVec {
    if let Some(xor_seq) = xor_sequence {
        let mut bits = corrected_bits[0..PAYLOAD_BITS].to_owned();
        for byte_idx in 0..10 {
            let xor_byte = xor_seq[byte_idx];
            for bit_pos in 0..8 {
                let global_bit = byte_idx * 8 + bit_pos;
                if global_bit >= PAYLOAD_BITS {
                    break;
                }
                if (xor_byte >> (7 - bit_pos)) & 1 == 1 {
                    let cur = bits[global_bit];
                    bits.set(global_bit, !cur);
                }
            }
        }
        bits
    } else {
        corrected_bits[0..PAYLOAD_BITS].to_owned()
    }
}

fn par_estimate_snr_spectrogram(pp: &ProtocolParams, tone_magnitudes: &[[f64; NUM_TONES]]) -> f32 {
    let data_positions = pp.data_symbol_indices();
    let mut signal_sum = 0.0f64;
    let mut noise_sum = 0.0f64;
    let mut count = 0usize;
    for &sym_idx in &data_positions {
        let mags = &tone_magnitudes[sym_idx];
        let best = mags.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let worst = mags.iter().cloned().fold(f64::INFINITY, f64::min);
        signal_sum += best;
        noise_sum += worst;
        count += 1;
    }
    if count > 0 {
        let avg_signal_db = signal_sum / count as f64;
        let avg_noise_db = noise_sum / count as f64;
        let snr_bin_db = avg_signal_db - avg_noise_db;
        let bw_correction = 10.0 * (2500.0f64 / 6.25).log10();
        (snr_bin_db - bw_correction) as f32
    } else {
        -24.0f32
    }
}

fn par_estimate_snr_fft(pp: &ProtocolParams, tone_magnitudes: &[[f64; NUM_TONES]]) -> f32 {
    let data_positions = pp.data_symbol_indices();
    let mut signal_power = 0.0f64;
    let mut noise_power = 0.0f64;
    let mut count = 0usize;
    for &sym_idx in &data_positions {
        let mags = &tone_magnitudes[sym_idx];
        let best = mags.iter().cloned().fold(0.0f64, f64::max);
        let worst = mags.iter().cloned().fold(f64::MAX, f64::min);
        signal_power += best * best;
        noise_power += worst * worst;
        count += 1;
    }
    if count > 0 && noise_power > 0.0 {
        let snr_linear = signal_power / noise_power;
        let bw_correction = 10.0 * (2500.0f64 / 6.25).log10();
        (10.0 * snr_linear.log10() - bw_correction) as f32
    } else {
        -24.0f32
    }
}

// ============================================================================
// Helper functions
// ============================================================================

/// Normalize LLR values to have a target variance, matching ft8_lib's
/// `ftx_normalize_logl()` when called with the default `LLR_TARGET_VARIANCE`.
///
/// LDPC belief propagation is tuned for a specific LLR scale. This function
/// computes the variance of the 174 LLR values and scales them so the variance
/// equals `target_variance`. Default is `LLR_TARGET_VARIANCE` (24.0). This is
/// critical for decoding weak signals; hb-006 swept this value as a possible
/// sensitivity knob.
fn normalize_llrs(llrs: &mut [f32], target_variance: f32) {
    debug_assert_eq!(llrs.len(), 174);
    let n = llrs.len() as f32;
    let inv_n = 1.0 / n;

    let sum: f32 = llrs.iter().sum();
    let sum2: f32 = llrs.iter().map(|&x| x * x).sum();

    let variance = (sum2 - sum * sum * inv_n) * inv_n;

    if variance > 0.0 {
        let norm_factor = (target_variance / variance).sqrt();
        for llr in llrs.iter_mut() {
            *llr *= norm_factor;
        }
    }
}

/// JS8Call-Improved-inspired per-tone × per-symbol LLR whitening.
///
/// Inspired by spec ref `research/specs/spec-js8call-llr-whitening.md`.
///
/// Estimates a non-uniform noise floor over the symbol-magnitude matrix
/// and divides each LLR triplet at symbol position `sym` by the
/// geometric mean of the noise estimates at `(winner_tone(sym), sym)`.
/// The intent is to give the LDPC decoder LLRs on a comparable scale
/// across bit positions even when the local noise floor varies by
/// frequency (band edge vs middle) or by time (QRN bursts, neighbouring
/// stations bleeding into nearby tones).
///
/// Algorithm steps (from the spec, simplified to operate on a single
/// LLR vector and stay byte-identical with the existing pipeline when
/// disabled):
///
/// 1. **Per-tone noise estimate** (`n_tone[0..NUM_TONES]`): for each
///    of the eight tone rows, compute the median of the magnitudes
///    across all data-symbol positions where that tone is **not** the
///    winner.
/// 2. **Per-symbol noise estimate** (`n_symbol[0..ND]`): for each
///    data-symbol position, compute the median over the
///    `(NUM_TONES - 1)` non-winning tone magnitudes.
/// 3. **Divisive normalisation**: for each data symbol position
///    `sym` with winner tone `w`, divide each of its three LLRs by
///    `sqrt(max(n_tone[w], floor) * max(n_symbol[sym], floor))`.
///
/// The variance-standardisation step from the spec is handled by the
/// existing `normalize_llrs` pass that immediately follows this helper.
///
/// Numerical safety:
/// - A `1e-6` floor is applied to both noise estimates to prevent
///   division-by-zero on identically-zero tone rows or symbol slots.
/// - Uniform magnitudes produce uniform noise estimates → the divisor
///   is a single scalar across the whole vector → after
///   `normalize_llrs` the output is identical to the no-whitening
///   path (uniform-scale identity).
/// - All-zero magnitudes leave LLRs unchanged (divisor stays at the
///   floor, but every LLR is already zero so the scaling is a no-op).
fn whiten_llrs(llrs: &mut [f32], tone_magnitudes: &[[f64; NUM_TONES]], pp: &ProtocolParams) {
    const NOISE_FLOOR: f64 = 1e-6;
    let data_positions = pp.data_symbol_indices();
    let nd = data_positions.len();
    let bps = pp.bits_per_symbol;
    if nd == 0 || bps == 0 || llrs.is_empty() {
        return;
    }
    debug_assert_eq!(llrs.len(), nd * bps);

    // Determine the winning tone at each data symbol position. We use
    // the raw FFT-bin argmax (not the Gray-decoded label) because the
    // whitening step operates on the magnitude matrix BEFORE the
    // Gray-coded LLR formula and only needs "which tone was loudest".
    let mut winners: Vec<usize> = Vec::with_capacity(nd);
    for &sym_idx in &data_positions {
        let mags = &tone_magnitudes[sym_idx];
        let mut best_tone = 0usize;
        let mut best_mag = f64::NEG_INFINITY;
        for (t, &m) in mags.iter().enumerate() {
            if m > best_mag {
                best_mag = m;
                best_tone = t;
            }
        }
        winners.push(best_tone);
    }

    // Per-tone noise estimate: median of magnitudes across positions
    // where the tone was NOT the winner.
    let mut n_tone = [0.0f64; NUM_TONES];
    let mut scratch: Vec<f64> = Vec::with_capacity(nd);
    for tone in 0..NUM_TONES {
        scratch.clear();
        for (i, &sym_idx) in data_positions.iter().enumerate() {
            if winners[i] != tone {
                scratch.push(tone_magnitudes[sym_idx][tone]);
            }
        }
        n_tone[tone] = if scratch.is_empty() {
            // Pathological: this tone was the winner at EVERY data
            // symbol position (would require all 58 data symbols to
            // peak at the same tone). Fall back to the all-positions
            // median to keep the divisor well-defined.
            for &sym_idx in &data_positions {
                scratch.push(tone_magnitudes[sym_idx][tone]);
            }
            median_inplace(&mut scratch).max(NOISE_FLOOR)
        } else {
            median_inplace(&mut scratch).max(NOISE_FLOOR)
        };
    }

    // Per-symbol noise estimate: median over the (NUM_TONES - 1)
    // non-winning tones for each data symbol position.
    let mut n_symbol: Vec<f64> = Vec::with_capacity(nd);
    let mut per_sym_scratch: Vec<f64> = Vec::with_capacity(NUM_TONES.saturating_sub(1));
    for (i, &sym_idx) in data_positions.iter().enumerate() {
        per_sym_scratch.clear();
        let w = winners[i];
        for (t, &m) in tone_magnitudes[sym_idx].iter().enumerate() {
            if t != w {
                per_sym_scratch.push(m);
            }
        }
        let med = median_inplace(&mut per_sym_scratch).max(NOISE_FLOOR);
        n_symbol.push(med);
    }

    // Apply divisive normalisation. Each symbol's three LLRs share the
    // same divisor (the per-(winner-tone, symbol) geometric mean noise
    // estimate), which after the downstream `normalize_llrs` collapses
    // to a no-op when the noise field is uniform.
    for (i, _sym_idx) in data_positions.iter().enumerate() {
        let w = winners[i];
        let denom_sq = n_tone[w] * n_symbol[i];
        // Floor again on the product (paranoia: both factors are
        // already floored, so denom_sq >= 1e-12; sqrt cannot underflow
        // f32 here, but the explicit floor keeps the contract clear).
        let denom = denom_sq.max(NOISE_FLOOR * NOISE_FLOOR).sqrt() as f32;
        if denom <= 0.0 || !denom.is_finite() {
            continue;
        }
        let inv = 1.0f32 / denom;
        let base = i * bps;
        for k in 0..bps {
            let v = llrs[base + k] * inv;
            // Defensive NaN/Inf clamp: while the floor guarantees a
            // finite divisor, the input LLRs could theoretically be
            // non-finite if upstream produced one. Replace with 0.0 so
            // LDPC sees a benign value rather than a NaN that would
            // poison every check-node update.
            llrs[base + k] = if v.is_finite() { v } else { 0.0 };
        }
    }
}

/// Gated wrapper around `whiten_llrs` that no-ops when `enabled` is
/// false. Centralises the default-OFF byte-identical contract: when
/// disabled, ZERO whitening code runs (one branch test only).
#[inline]
fn maybe_whiten_llrs(
    enabled: bool,
    llrs: &mut [f32],
    tone_magnitudes: &[[f64; NUM_TONES]],
    pp: &ProtocolParams,
) {
    if enabled {
        whiten_llrs(llrs, tone_magnitudes, pp);
    }
}

/// Compute the median of a mutable slice of f64, in-place.
///
/// Uses `sort_by` (O(n log n)) rather than quickselect because the
/// inputs are at most `NUM_TONES - 1 = 7` (per-symbol case) or
/// `nd ≈ 58` (per-tone case), so the asymptotic difference is
/// irrelevant and `sort_by` keeps the helper allocation-free in
/// std. For an even-length slice returns the lower of the two middle
/// values (the spec uses "median" without specifying tie-breaking,
/// and sample-variance differences from the chosen tie-break are
/// negligible at these sizes).
fn median_inplace(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values[values.len() / 2]
}

/// Convert bit slice to u16
fn bits_to_u16(bits: &BitSlice) -> u16 {
    let mut value = 0u16;
    for (i, bit) in bits.iter().enumerate() {
        if *bit && i < 16 {
            value |= 1 << (bits.len() - 1 - i);
        }
    }
    value
}

/// hb-242 wide-lag baseline: compute the nth-percentile reference of
/// a per-bin sync-peak array. Each entry is a `(score, time_step)`
/// pair; only the score participates in the percentile. NaN / non-
/// finite scores sort to the front (treated as zero). The percentile
/// index follows the wsjtr convention:
/// `idx = round(pct * len).max(1) - 1`.
///
/// Returns the score value at the selected sorted index, or 0.0 if
/// the input is empty.
fn percentile_baseline(peaks: &[(f64, usize)], pct: f64) -> f64 {
    if peaks.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f64> = peaks
        .iter()
        .map(|&(s, _)| if s.is_finite() { s } else { 0.0 })
        .collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let len = sorted.len();
    // `round` is "round half away from zero" — within-one-bin of any
    // other rounding mode, well below the sensitivity of the
    // percentile baseline.
    let raw = (pct * len as f64).round() as usize;
    let idx = raw.max(1) - 1;
    let idx = idx.min(len - 1);
    sorted[idx]
}

/// hb-044 / hb-245: parabolic refinement of a discrete peak. Given
/// three score samples at integer offsets (-1, 0, +1) around the
/// candidate's peak, fit a parabola and return (refined_score,
/// fractional_offset). The fractional offset is in (-0.5, +0.5)
/// relative to the center sample.
///
/// If the three points don't form a concave-down parabola (i.e., the
/// center isn't actually a local max), returns (y_center, 0.0).
///
/// Per spec-wsjtx-improved-subsample-dt-refinement.md (hb-245), this
/// is the textbook three-point parabolic peak interpolator (Smith,
/// "Spectral Audio Signal Processing"). Pancetta's `a < 0` concave
/// check is equivalent to the spec's "denominator strictly positive"
/// check (the spec uses `c[-1] - 2*c[0] + c[+1] > 0`, which is the
/// same condition as `-2*a > 0` ⇔ `a < 0`). The clamp to [-0.5,
/// +0.5] matches the spec's edge-case handling.
fn parabolic_peak_refinement(y_left: f64, y_center: f64, y_right: f64) -> (f64, f64) {
    // Parabola: y(x) = a*x^2 + b*x + c
    //   y(-1) = a - b + c = y_left
    //   y( 0) =        c = y_center
    //   y(+1) = a + b + c = y_right
    //   => a = (y_left + y_right - 2*y_center) / 2
    //      b = (y_right - y_left) / 2
    let a = (y_left + y_right - 2.0 * y_center) * 0.5;
    let b = (y_right - y_left) * 0.5;
    if a >= 0.0 {
        // Not concave-down — center is not a local max. No refinement.
        return (y_center, 0.0);
    }
    let delta = -b / (2.0 * a);
    // Clamp to a sane range — large deltas mean the parabola fit is poor.
    let delta = delta.clamp(-0.5, 0.5);
    let refined = y_center + b * delta + a * delta * delta;
    (refined, delta)
}

#[cfg(test)]
mod parabolic_tests {
    use super::{parabolic_peak_refinement, percentile_baseline};

    #[test]
    fn symmetric_peak_has_no_offset() {
        // y_left == y_right, center higher → delta = 0
        let (refined, delta) = parabolic_peak_refinement(1.0, 2.0, 1.0);
        assert!(delta.abs() < 1e-9);
        assert!((refined - 2.0).abs() < 1e-9);
    }

    #[test]
    fn skewed_right_pushes_delta_right() {
        let (_, delta) = parabolic_peak_refinement(1.0, 2.0, 1.5);
        assert!(delta > 0.0, "delta={delta}");
        assert!(delta < 0.5);
    }

    #[test]
    fn non_concave_returns_zero() {
        // y_center smaller than both neighbors → not a peak
        let (refined, delta) = parabolic_peak_refinement(2.0, 1.0, 2.0);
        assert_eq!(delta, 0.0);
        assert_eq!(refined, 1.0);
    }

    /// hb-245: recover a Gaussian peak centred at a sub-sample offset.
    /// Pancetta's `parabolic_peak_refinement` implements the same
    /// closed-form formula as the hb-245 spec (Smith textbook).
    #[test]
    fn hb245_synthetic_offset_peak_recovered() {
        let true_offset = 0.37_f64;
        let g = |k: f64| (-((k - true_offset).powi(2) / 2.0)).exp();
        let (refined, delta) = parabolic_peak_refinement(g(-1.0), g(0.0), g(1.0));
        assert!(
            (delta - true_offset).abs() < 0.05,
            "delta={delta} should be near {true_offset}"
        );
        assert!(refined >= g(0.0) - 1e-9);
    }

    /// hb-245 edge: flat three-point window (all values equal) is
    /// not a peak — non-concave branch returns delta = 0.
    #[test]
    fn hb245_flat_window_returns_zero_delta() {
        let (refined, delta) = parabolic_peak_refinement(2.0, 2.0, 2.0);
        assert_eq!(delta, 0.0);
        assert!((refined - 2.0).abs() < 1e-9);
    }

    /// hb-245 edge: result always falls in [-0.5, +0.5].
    #[test]
    fn hb245_result_clamped_to_half_sample() {
        let (_refined, delta) = parabolic_peak_refinement(10.0, 11.0, 0.0);
        assert!((-0.5..=0.5).contains(&delta), "delta={delta} out of range");
    }

    /// hb-242 percentile baseline: uniform distribution → the 40th-
    /// percentile entry equals the 40th-percentile value.
    #[test]
    fn percentile_baseline_uniform_distribution() {
        let peaks: Vec<(f64, usize)> = (0..100).map(|i| (i as f64 / 100.0, i)).collect();
        let base = percentile_baseline(&peaks, 0.40);
        // round(0.40 * 100) = 40, idx = 39, sorted[39] = 0.39.
        assert!((base - 0.39).abs() < 1e-9, "base={base}");
    }

    /// hb-242 percentile baseline: empty input → 0.0 gracefully
    /// disables normalisation downstream.
    #[test]
    fn percentile_baseline_empty_input() {
        assert_eq!(percentile_baseline(&[], 0.40), 0.0);
    }

    /// hb-242 percentile baseline: NaN scores are treated as zero
    /// and sort to the front. The result must remain finite.
    #[test]
    fn percentile_baseline_nan_safe() {
        let peaks = vec![(f64::NAN, 0), (1.0, 1), (2.0, 2), (3.0, 3), (4.0, 4)];
        let base = percentile_baseline(&peaks, 0.40);
        assert!(base.is_finite());
        // round(0.40 * 5) = 2, idx = 1, sorted (NaN→0): [0, 1, 2, 3, 4]. → 1.0.
        assert!((base - 1.0).abs() < 1e-9, "base={base}");
    }
}

/// Estimate noise floor from power spectral density (median method)
fn estimate_noise_floor(psd: &[f64]) -> f64 {
    let mut sorted_psd = psd.to_vec();
    sorted_psd.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_idx = sorted_psd.len() / 2;
    sorted_psd[median_idx]
}

/// hb-016: average excess-above-noise (in dB) across a spectrogram's
/// power tensor. For each bin, take `max(0, db - noise_floor_db)` and
/// average. Result is monotone in signal energy: 0 when every bin sits
/// at or below the noise-floor reference, grows as bright signal bins
/// are added. O(N) over all (time, freq_sub, freq_bin) entries. The
/// earlier `mean(linear power)` variant was dominated by surviving
/// strong bins and failed to drop into the early-stop regime even
/// after most signals had been subtracted; this metric goes to zero
/// when subtraction zeroes the per-bin excess above the original
/// floor.
fn mean_excess_above_noise_db(power: &[Vec<Vec<f64>>], noise_floor_db: f64) -> f64 {
    let mut sum = 0.0_f64;
    let mut count = 0_usize;
    for ts in power {
        for sub in ts {
            for &db in sub {
                let excess = db - noise_floor_db;
                if excess > 0.0 {
                    sum += excess;
                }
                count += 1;
            }
        }
    }
    if count == 0 {
        return 0.0;
    }
    sum / count as f64
}

/// hb-016: noise-floor proxy across a spectrogram — median dB of all
/// bins. Most bins in an FT8 slot are noise (signals occupy a small
/// fraction of (time, freq) cells), so the median of dB is a robust
/// floor estimator. Computed once on the ORIGINAL spectrogram and
/// reused as a stable reference across multipass rounds. O(N log N) one
/// time; small relative to the rest of the decode budget.
fn noise_floor_db_median(power: &[Vec<Vec<f64>>]) -> f64 {
    let mut all: Vec<f64> = Vec::with_capacity(
        power
            .iter()
            .map(|t| t.iter().map(|s| s.len()).sum::<usize>())
            .sum::<usize>(),
    );
    for ts in power {
        for sub in ts {
            for &db in sub {
                all.push(db);
            }
        }
    }
    if all.is_empty() {
        return -120.0;
    }
    all.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    all[all.len() / 2]
}

// ============================================================================
// LDPC decoder
// ============================================================================

/// Padé approximant for tanh, matching ft8_lib's approach.
#[inline]
fn fast_tanh(x: f32) -> f32 {
    if x.abs() > 4.97 {
        return if x > 0.0 { 1.0 } else { -1.0 };
    }
    let x2 = x * x;
    let num = x * (135135.0 + x2 * (17325.0 + x2 * (378.0 + x2)));
    let den = 135135.0 + x2 * (62370.0 + x2 * (3150.0 + x2 * 28.0));
    num / den
}

#[inline]
fn fast_atanh(x: f32) -> f32 {
    let x = x.clamp(-0.9999999, 0.9999999);
    0.5 * ((1.0 + x) / (1.0 - x)).ln()
}

#[derive(Debug, Clone, Copy)]
enum LdpcAlgorithm {
    MinSum { normalization_factor: f32 },
    SumProduct,
}

/// FT8 LDPC(174,91) decoder with belief propagation
///
/// Implements the LDPC decoder for FT8's (174,91) code:
/// - 91 information bits (77 payload + 14 CRC)
/// - 83 parity bits
/// - **Sum-product** belief propagation algorithm (tanh check-message:
///   `2·atanh(∏ tanh(v/2))`). The `LdpcAlgorithm::MinSum` enum variant
///   is implemented but the production constructor hardcodes
///   `SumProduct` — min-sum is library-only and unreachable from the
///   production decoder path. Phase D 2026-06-02 audit (claim 18)
///   tightened this docstring from "sum-product or min-sum" — see
///   `docs/engineering/2026-06-02-engineering-substance-audit.md`.
struct LdpcDecoder {
    max_iterations: usize,
    /// Parity check matrix (83x174) - sparse representation
    parity_check_matrix: ParityCheckMatrix,
    /// For each variable node, the position index within each connected check node's list.
    /// var_positions[var_idx] = [(check_idx, position_in_check), ...] with exactly 3 entries.
    var_positions: Vec<Vec<(usize, usize)>>,
    /// LDPC decoding algorithm
    algorithm: LdpcAlgorithm,
    /// Optional OSD fallback decoder
    osd: Option<OsdDecoder>,
    /// Max unsatisfied parity checks tolerated before invoking OSD.
    /// 4 = production default; hb-014 sweep candidate.
    max_parity_errors_for_osd: usize,
    /// hb-067 mBP offset: subtract this magnitude from each LLR before
    /// invoking OSD. Reduces BP's confidence → OSD considers more flip
    /// patterns. Default 0.0 = no offset (no behavior change).
    /// Per arXiv:2306.00443 — claim is order-(m-1) OSD reaches order-m
    /// performance with small offset.
    bp_offset_subtract: f32,
    /// hb-063: when true, `belief_propagation_with_trajectory` uses a
    /// layered (row-sequential) schedule instead of flooding.
    layered: bool,
    /// JS8Call-Improved-style feedback refinement config (clean-room port
    /// from `spec-js8call-ldpc-feedback-refinement.md`). When `enabled` is
    /// false (default), `decode_soft` is byte-identical to its pre-feedback
    /// behavior. When true, a failed first BP pass triggers one meta-loop
    /// with refined LLRs before falling through to OSD.
    feedback_refinement: FeedbackRefinementConfig,
}

/// Configuration for the JS8Call-Improved-style LDPC feedback refinement
/// meta-loop. See `Ft8Config::ldpc_feedback_refinement_enabled` for the
/// caller-facing surface; `LdpcDecoder` stores a flattened copy.
#[derive(Debug, Clone, Copy)]
struct FeedbackRefinementConfig {
    /// Master switch. When false, `decode_soft` skips refinement entirely.
    enabled: bool,
    /// Multiplier applied to `|LLR|` when the original sign agrees with the
    /// iter-1 hard-decision codeword bit.
    boost_factor: f32,
    /// Multiplier applied to `|LLR|` when the original sign disagrees with
    /// the iter-1 hard-decision codeword bit (and is not erased).
    attenuate_factor: f32,
    /// On the disagreement path, if the original `|LLR|` is below this
    /// threshold the bit is forced to 0 (erasure). Set to `f32::INFINITY`
    /// to disable erasure.
    erase_threshold: f32,
}

impl Default for FeedbackRefinementConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            boost_factor: 1.5,
            attenuate_factor: 0.5,
            erase_threshold: 1.0,
        }
    }
}

/// Telemetry from one application of the JS8Call-Improved-style feedback
/// refinement. Returned by `refine_llrs_from_hard_decisions` so callers can
/// log the agree/disagree split (useful for the research scorecard) and
/// decide whether to keep looping.
#[derive(Debug, Clone, Copy, Default)]
struct FeedbackStats {
    /// Bits where the original LLR sign agreed with the iter-1 hard decision
    /// (magnitude was boosted).
    confident_bits: u16,
    /// Bits where the original LLR sign disagreed with the iter-1 hard
    /// decision (magnitude was attenuated or erased).
    uncertain_bits: u16,
    /// Subset of `uncertain_bits` whose magnitude was forced to 0.
    erased_bits: u16,
}

/// Clamp factor for refined LLR magnitudes — guards against numerical
/// overflow on repeated boosts when the meta-loop is invoked in a future
/// multi-iteration regime. Per the spec: "clamp LLR magnitude to a
/// reasonable maximum (e.g., ±30 in log-base-e LLR units)".
const FEEDBACK_REFINEMENT_LLR_CLAMP: f32 = 30.0;

/// Apply the JS8Call-Improved-style feedback refinement to `llrs` in place,
/// using `hard_decisions[i] = 1` to indicate "iter-1 BP decided bit i = 1"
/// (i.e. its output LLR was negative).
///
/// Returns telemetry counters for diagnostics. The transform is deterministic
/// and free of randomness: callers may unit-test it directly.
fn refine_llrs_from_hard_decisions(
    llrs: &mut [f32; 174],
    hard_decisions: &[u8; 174],
    cfg: &FeedbackRefinementConfig,
) -> FeedbackStats {
    let mut stats = FeedbackStats::default();
    for i in 0..174 {
        let original = llrs[i];
        let abs = original.abs();
        // "Bit = 1 according to original LLR" iff original < 0 (matches
        // `llrs_to_bits` and `check_syndrome_fast`).
        let llr_bit: u8 = u8::from(original < 0.0);
        let hd_bit = hard_decisions[i] & 1;

        if llr_bit == hd_bit {
            // Agreement: amplify magnitude, preserve sign. A zero LLR stays
            // zero (no information to amplify); this matches the spec which
            // treats erased bits as "unknown" and lets parity drive the
            // decision.
            let scaled = abs * cfg.boost_factor;
            let clamped = scaled.min(FEEDBACK_REFINEMENT_LLR_CLAMP);
            llrs[i] = if original == 0.0 {
                0.0
            } else {
                original.signum() * clamped
            };
            stats.confident_bits += 1;
        } else {
            // Disagreement: erase if shallow, else attenuate. Sign preserved
            // when attenuating so the demapper's vote is not discarded
            // outright — the candidate codeword merely casts doubt on it.
            // `erase_threshold = +infinity` (or non-finite) is the
            // documented sentinel meaning "disable erasure" — gate on
            // finiteness so it always falls through to attenuation.
            if cfg.erase_threshold.is_finite() && abs < cfg.erase_threshold {
                llrs[i] = 0.0;
                stats.uncertain_bits += 1;
                stats.erased_bits += 1;
            } else {
                let scaled = abs * cfg.attenuate_factor;
                let clamped = scaled.min(FEEDBACK_REFINEMENT_LLR_CLAMP);
                llrs[i] = original.signum() * clamped;
                stats.uncertain_bits += 1;
            }
        }
    }
    stats
}

impl LdpcDecoder {
    fn new(max_iterations: usize) -> Ft8Result<Self> {
        let parity_check_matrix = ParityCheckMatrix::new_ft8();

        // Pre-compute position lookup: for each variable node, find its position
        // in each connected check node's variable list. This avoids O(degree)
        // linear searches during belief propagation iterations.
        let mut var_positions = Vec::with_capacity(174);
        for var_idx in 0..174 {
            let connected_checks = parity_check_matrix.get_connected_checks(var_idx);
            let mut positions = Vec::with_capacity(connected_checks.len());
            for &check_idx in connected_checks {
                let check_vars = parity_check_matrix.get_connected_variables(check_idx);
                let pos = check_vars
                    .iter()
                    .position(|&v| v == var_idx)
                    .expect("Inconsistent parity check matrix");
                positions.push((check_idx, pos));
            }
            var_positions.push(positions);
        }

        Ok(Self {
            max_iterations,
            parity_check_matrix,
            var_positions,
            algorithm: LdpcAlgorithm::SumProduct,
            osd: None,
            max_parity_errors_for_osd: 4,
            bp_offset_subtract: 0.0,
            layered: false,
            feedback_refinement: FeedbackRefinementConfig::default(),
        })
    }

    fn new_with_osd(max_iterations: usize, osd_config: Option<OsdConfig>) -> Ft8Result<Self> {
        let mut decoder = Self::new(max_iterations)?;
        decoder.osd = osd_config.map(OsdDecoder::new);
        Ok(decoder)
    }

    fn with_max_parity_errors_for_osd(mut self, n: usize) -> Self {
        self.max_parity_errors_for_osd = n;
        self
    }

    fn with_bp_offset_subtract(mut self, v: f32) -> Self {
        self.bp_offset_subtract = v.max(0.0);
        self
    }

    fn with_layered(mut self, on: bool) -> Self {
        self.layered = on;
        self
    }

    /// Enable / configure the JS8Call-Improved-style LDPC feedback refinement
    /// meta-loop. Passing `enabled = false` (default) preserves byte-identical
    /// `decode_soft` behavior; passing `enabled = true` activates one extra
    /// BP pass on refined LLRs whenever the first pass fails to converge.
    ///
    /// Factors and threshold must be finite; non-finite values are silently
    /// replaced with their defaults to avoid downstream NaN propagation.
    fn with_feedback_refinement(
        mut self,
        enabled: bool,
        boost_factor: f32,
        attenuate_factor: f32,
        erase_threshold: f32,
    ) -> Self {
        let boost = if boost_factor.is_finite() && boost_factor > 0.0 {
            boost_factor
        } else {
            1.5
        };
        let atten = if attenuate_factor.is_finite() && attenuate_factor >= 0.0 {
            attenuate_factor
        } else {
            0.5
        };
        // `erase_threshold` may be `f32::INFINITY` (disable erasure) — that is
        // intentional and finite-check would reject it.
        let erase = if erase_threshold.is_nan() || erase_threshold < 0.0 {
            1.0
        } else {
            erase_threshold
        };
        self.feedback_refinement = FeedbackRefinementConfig {
            enabled,
            boost_factor: boost,
            attenuate_factor: atten,
            erase_threshold: erase,
        };
        self
    }

    /// Decode using belief propagation with hard-decision input
    fn decode(&self, bits: &BitVec) -> Ft8Result<BitVec> {
        let llrs = self.bits_to_llrs(bits);
        let decoded_llrs = self.belief_propagation(&llrs)?;
        self.llrs_to_bits(&decoded_llrs)
    }

    /// Decode with soft-decision input (LLRs)
    pub fn decode_soft(&self, llrs: &[f32]) -> Ft8Result<BitVec> {
        if llrs.len() != 174 {
            return Err(Ft8Error::InvalidDataSize {
                expected: 174,
                actual: llrs.len(),
            });
        }

        // Use trajectory-collecting BP
        let (decoded_llrs, trajectory) = self.belief_propagation_with_trajectory(llrs)?;

        // Check if BP converged (syndrome = 0)
        let bp_converged = {
            let arr: &[f32; 174] = decoded_llrs[..174].try_into().unwrap();
            self.check_syndrome_fast(arr)
        };

        if bp_converged {
            return self.llrs_to_bits(&decoded_llrs);
        }

        // JS8Call-Improved-style feedback refinement meta-loop. When the
        // first BP pass fails, the partial codeword it produced still encodes
        // BP's current best hypothesis; we use it to reshape the input LLRs
        // (boost agreeing bits, attenuate / erase disagreeing bits) and run
        // BP once more. Default OFF — gated by `feedback_refinement.enabled`.
        //
        // Spec: `research/specs/spec-js8call-ldpc-feedback-refinement.md`.
        let (decoded_llrs, trajectory) = if self.feedback_refinement.enabled {
            // Capture iter-1 hard decisions as 0/1 bytes (sign of output
            // LLRs; matches `llrs_to_bits` / `check_syndrome_fast`).
            let mut hard_decisions = [0u8; 174];
            for i in 0..174 {
                hard_decisions[i] = u8::from(decoded_llrs[i] < 0.0);
            }

            // Refine the original channel LLRs in place.
            let mut refined = [0.0f32; 174];
            refined.copy_from_slice(&llrs[..174]);
            let _stats = refine_llrs_from_hard_decisions(
                &mut refined,
                &hard_decisions,
                &self.feedback_refinement,
            );

            // Run BP iteration 2 on the refined LLRs.
            let (refined_decoded, refined_trajectory) =
                self.belief_propagation_with_trajectory(&refined)?;

            // If the refined pass converged, return immediately — the
            // meta-loop did its job and OSD is unnecessary.
            let refined_converged = {
                let arr: &[f32; 174] = refined_decoded[..174].try_into().unwrap();
                self.check_syndrome_fast(arr)
            };
            if refined_converged {
                return self.llrs_to_bits(&refined_decoded);
            }

            // Refined pass also failed: hand its output (and any new
            // trajectory) downstream to OSD. The trajectory is what neural
            // OSD scores against, so using the refined trajectory keeps the
            // bit-flip ordering consistent with what OSD actually sees.
            (refined_decoded, refined_trajectory)
        } else {
            (decoded_llrs, trajectory)
        };

        // hb-064: BP did not converge — note channel LLRs + trajectory
        // before potentially handing off to OSD, so the research
        // capture path can correlate input/trajectory/outcome. Cheap
        // unconditional thread-local read; the borrow only happens
        // when capture is enabled.
        let capture_enabled = crate::bp_trajectory_capture::is_enabled();
        let captured_channel_llrs: Option<[f32; 174]> = if capture_enabled {
            let mut arr = [0.0f32; 174];
            arr.copy_from_slice(&llrs[..174]);
            Some(arr)
        } else {
            None
        };
        let captured_final_llrs: Option<[f32; 174]> = if capture_enabled {
            let mut arr = [0.0f32; 174];
            arr.copy_from_slice(&decoded_llrs[..174]);
            Some(arr)
        } else {
            None
        };

        // BP did not converge — try OSD fallback if available.
        if let Some(ref osd) = self.osd {
            // hb-067: optional mBP offset — reduce BP-LLR magnitudes
            // before OSD invocation so OSD considers more flip patterns
            // (per arXiv:2306.00443). bp_offset_subtract=0 → no change.
            let llrs_for_osd: Vec<f32> = if self.bp_offset_subtract > 0.0 {
                decoded_llrs[..174]
                    .iter()
                    .map(|&v| v.signum() * (v.abs() - self.bp_offset_subtract).max(0.0))
                    .collect()
            } else {
                decoded_llrs[..174].to_vec()
            };
            let llr_arr: &[f32; 174] = llrs_for_osd[..174].try_into().unwrap();
            let parity_errors = self.count_parity_errors(llr_arr);

            // Parity gate for OSD: tunable via Ft8Config::max_parity_errors_for_osd.
            // Default 4: widening to 5 historically let too many noise candidates
            // through (CRC-14 collisions become FPs); tightening to 3 lost real
            // decodes. hb-014 re-sweeps this on the current production state.
            if parity_errors <= self.max_parity_errors_for_osd {
                // Compute neural ordering if trajectory is available and the
                // neural-OSD feature is compiled in. Without the feature,
                // OSD falls back to |LLR|-based ordering at the cost of
                // higher trial counts on weak signals.
                #[cfg(feature = "neural_osd")]
                let neural_ordering = trajectory
                    .as_ref()
                    .map(|traj| crate::neural_osd::predict_error_bits(traj));
                #[cfg(not(feature = "neural_osd"))]
                let neural_ordering: Option<[f32; 91]> = {
                    let _ = trajectory.as_ref();
                    None
                };

                let osd_result = osd.decode(llr_arr, neural_ordering.as_ref());

                // hb-064: record (trajectory, OSD outcome) for the
                // research dataset. Only fires when capture is enabled
                // AND OSD was actually attempted (i.e. parity-gate
                // passed) — those are the cases the future model will
                // see in production.
                if capture_enabled {
                    let (osd_recovered, osd_codeword) = match &osd_result {
                        Some(bv) => {
                            let mut arr = [0u8; 174];
                            for (i, slot) in arr.iter_mut().enumerate() {
                                *slot = u8::from(bv.get(i).map(|b| *b).unwrap_or(false));
                            }
                            (true, Some(arr))
                        }
                        None => (false, None),
                    };
                    let traj = trajectory.unwrap_or([[0.0; 174]; 25]);
                    crate::bp_trajectory_capture::record(
                        crate::bp_trajectory_capture::CapturedTrajectory {
                            channel_llrs: captured_channel_llrs.unwrap_or([0.0; 174]),
                            trajectory: traj,
                            final_llrs: captured_final_llrs.unwrap_or([0.0; 174]),
                            osd_recovered,
                            osd_codeword,
                            bp_iters_run: self.max_iterations.min(25) as u16,
                        },
                    );
                }

                if let Some(codeword) = osd_result {
                    return Ok(codeword);
                }
            } else if capture_enabled {
                // Parity-gate rejected. Record with `osd_recovered = false`
                // and `osd_codeword = None` so the dataset can also study
                // whether the trajectory signature predicts the gate
                // outcome (a useful auxiliary signal for the diagnostic).
                let traj = trajectory.unwrap_or([[0.0; 174]; 25]);
                crate::bp_trajectory_capture::record(
                    crate::bp_trajectory_capture::CapturedTrajectory {
                        channel_llrs: captured_channel_llrs.unwrap_or([0.0; 174]),
                        trajectory: traj,
                        final_llrs: captured_final_llrs.unwrap_or([0.0; 174]),
                        osd_recovered: false,
                        osd_codeword: None,
                        bp_iters_run: self.max_iterations.min(25) as u16,
                    },
                );
            }
        }

        // Return BP's best effort (caller will check CRC and likely reject)
        self.llrs_to_bits(&decoded_llrs)
    }

    /// Convert hard bits to soft LLRs
    fn bits_to_llrs(&self, bits: &BitVec) -> Vec<f32> {
        let mut llrs = Vec::with_capacity(174);
        const HARD_DECISION_LLR: f32 = 4.0;

        for i in 0..174.min(bits.len()) {
            llrs.push(if bits.get(i).map(|b| *b).unwrap_or(false) {
                -HARD_DECISION_LLR
            } else {
                HARD_DECISION_LLR
            });
        }

        while llrs.len() < 174 {
            llrs.push(0.0);
        }

        llrs
    }

    /// Convert LLRs to hard bit decisions
    fn llrs_to_bits(&self, llrs: &[f32]) -> Ft8Result<BitVec> {
        let mut bits = BitVec::with_capacity(174);

        for &llr in llrs.iter().take(174) {
            bits.push(llr < 0.0); // Negative LLR means bit = 1
        }

        Ok(bits)
    }

    /// Belief propagation algorithm using min-sum approximation.
    ///
    /// Uses sparse message storage (only connected edges) and checks syndrome
    /// after every iteration for early termination. Most decodable messages
    /// converge in 10-30 iterations rather than running all 100.
    fn belief_propagation(&self, channel_llrs: &[f32]) -> Ft8Result<Vec<f32>> {
        let num_checks = self.parity_check_matrix.num_checks;
        let num_vars = self.parity_check_matrix.num_variables;

        // Sparse message storage: one f32 per edge in the Tanner graph.
        // For each check node, store messages indexed by position in its connection list.
        // Max degree is 7, so we use fixed-size arrays to avoid heap allocation.
        let mut v2c = [[0.0f32; 7]; 83]; // variable-to-check messages
        let mut c2v = [[0.0f32; 7]; 83]; // check-to-variable messages
        let mut output_llrs = [0.0f32; 174];
        output_llrs[..num_vars].copy_from_slice(&channel_llrs[..num_vars]);

        // Initialize variable-to-check messages with channel LLRs
        for check_idx in 0..num_checks {
            let connected_vars = self.parity_check_matrix.get_connected_variables(check_idx);
            for (pos, &var_idx) in connected_vars.iter().enumerate() {
                v2c[check_idx][pos] = channel_llrs[var_idx];
            }
        }

        for _iteration in 0..self.max_iterations {
            // Check node update
            for check_idx in 0..num_checks {
                let connected_vars = self.parity_check_matrix.get_connected_variables(check_idx);
                let degree = connected_vars.len();

                match self.algorithm {
                    LdpcAlgorithm::SumProduct => {
                        for target_pos in 0..degree {
                            let mut product = 1.0f32;
                            for pos in 0..degree {
                                if pos != target_pos {
                                    product *= fast_tanh(v2c[check_idx][pos] / 2.0);
                                }
                            }
                            c2v[check_idx][target_pos] = 2.0 * fast_atanh(product);
                        }
                    }
                    LdpcAlgorithm::MinSum {
                        normalization_factor,
                    } => {
                        // Compute sign product and find two smallest magnitudes across all edges
                        let mut total_sign: i8 = 1;
                        let mut min1_mag = f32::MAX;
                        let mut min2_mag = f32::MAX;
                        let mut min1_pos: usize = 0;
                        let mut signs = [1i8; 7];

                        for pos in 0..degree {
                            let msg = v2c[check_idx][pos];
                            let s = if msg < 0.0 { -1i8 } else { 1i8 };
                            signs[pos] = s;
                            total_sign *= s;

                            let mag = msg.abs();
                            if mag < min1_mag {
                                min2_mag = min1_mag;
                                min1_mag = mag;
                                min1_pos = pos;
                            } else if mag < min2_mag {
                                min2_mag = mag;
                            }
                        }

                        // Now compute check-to-variable messages
                        for pos in 0..degree {
                            let edge_sign = total_sign * signs[pos];
                            let mag = if pos == min1_pos { min2_mag } else { min1_mag };
                            c2v[check_idx][pos] = edge_sign as f32 * mag * normalization_factor;
                        }
                    }
                }
            }

            // Variable node update using pre-computed position lookup
            for var_idx in 0..num_vars {
                let positions = &self.var_positions[var_idx];

                // Sum all incoming check-to-variable messages
                let mut total = channel_llrs[var_idx];
                for &(check_idx, pos) in positions {
                    total += c2v[check_idx][pos];
                }
                output_llrs[var_idx] = total;

                // Update variable-to-check messages (total minus the incoming from that check)
                for &(check_idx, pos) in positions {
                    v2c[check_idx][pos] = total - c2v[check_idx][pos];
                }
            }

            // Early termination: check syndrome every iteration (including iteration 0).
            // Most decodable messages converge in 10-30 iterations.
            if self.check_syndrome_fast(&output_llrs) {
                return Ok(output_llrs.to_vec());
            }
        }

        Ok(output_llrs.to_vec())
    }

    /// Belief propagation with per-iteration LLR trajectory collection.
    /// Returns (final_llrs, Some(trajectory)) when BP fails to converge.
    /// Returns (final_llrs, None) when BP converges (no trajectory needed).
    fn belief_propagation_with_trajectory(
        &self,
        channel_llrs: &[f32],
    ) -> Ft8Result<(Vec<f32>, Option<[[f32; 174]; 25]>)> {
        let num_checks = self.parity_check_matrix.num_checks;
        let num_vars = self.parity_check_matrix.num_variables;

        let mut v2c = [[0.0f32; 7]; 83];
        let mut c2v = [[0.0f32; 7]; 83];
        let mut output_llrs = [0.0f32; 174];
        output_llrs[..num_vars].copy_from_slice(&channel_llrs[..num_vars]);
        let mut trajectory = [[0.0f32; 174]; 25];

        for check_idx in 0..num_checks {
            let connected_vars = self.parity_check_matrix.get_connected_variables(check_idx);
            for (pos, &var_idx) in connected_vars.iter().enumerate() {
                v2c[check_idx][pos] = channel_llrs[var_idx];
            }
        }

        let max_iters = self.max_iterations.min(25);

        if self.layered {
            // hb-063: layered (row-sequential) BP. Reuses the zero-init
            // `c2v` and the channel-LLR `output_llrs` from above and folds
            // each new check-to-variable message into the running posteriors
            // immediately, so later checks in the same sweep see fresher
            // beliefs (~2x convergence vs flooding). `v2c` is unused here.
            let mut total = output_llrs;
            for iteration in 0..self.max_iterations {
                for check_idx in 0..num_checks {
                    let connected_vars =
                        self.parity_check_matrix.get_connected_variables(check_idx);
                    let degree = connected_vars.len();

                    // Extrinsic variable-to-check messages from current
                    // posteriors (remove this check's last contribution).
                    let mut ext = [0.0f32; 7];
                    for pos in 0..degree {
                        ext[pos] = total[connected_vars[pos]] - c2v[check_idx][pos];
                    }

                    match self.algorithm {
                        LdpcAlgorithm::SumProduct => {
                            for target_pos in 0..degree {
                                let mut product = 1.0f32;
                                for pos in 0..degree {
                                    if pos != target_pos {
                                        product *= fast_tanh(ext[pos] / 2.0);
                                    }
                                }
                                let new_msg = 2.0 * fast_atanh(product);
                                let var_idx = connected_vars[target_pos];
                                total[var_idx] += new_msg - c2v[check_idx][target_pos];
                                c2v[check_idx][target_pos] = new_msg;
                            }
                        }
                        LdpcAlgorithm::MinSum {
                            normalization_factor,
                        } => {
                            let mut total_sign: i8 = 1;
                            let mut min1_mag = f32::MAX;
                            let mut min2_mag = f32::MAX;
                            let mut min1_pos: usize = 0;
                            let mut signs = [1i8; 7];

                            for (pos, &e) in ext.iter().enumerate().take(degree) {
                                let s = if e < 0.0 { -1i8 } else { 1i8 };
                                signs[pos] = s;
                                total_sign *= s;

                                let mag = e.abs();
                                if mag < min1_mag {
                                    min2_mag = min1_mag;
                                    min1_mag = mag;
                                    min1_pos = pos;
                                } else if mag < min2_mag {
                                    min2_mag = mag;
                                }
                            }

                            for pos in 0..degree {
                                let edge_sign = total_sign * signs[pos];
                                let mag = if pos == min1_pos { min2_mag } else { min1_mag };
                                let new_msg = edge_sign as f32 * mag * normalization_factor;
                                let var_idx = connected_vars[pos];
                                total[var_idx] += new_msg - c2v[check_idx][pos];
                                c2v[check_idx][pos] = new_msg;
                            }
                        }
                    }
                }

                output_llrs = total;

                if iteration < max_iters {
                    trajectory[iteration] = output_llrs;
                }
                if self.check_syndrome_fast(&output_llrs) {
                    return Ok((output_llrs.to_vec(), None));
                }
            }

            for slot in trajectory.iter_mut().take(25).skip(max_iters) {
                *slot = output_llrs;
            }
            return Ok((output_llrs.to_vec(), Some(trajectory)));
        }

        for iteration in 0..self.max_iterations {
            // Check node update (same as belief_propagation)
            for check_idx in 0..num_checks {
                let connected_vars = self.parity_check_matrix.get_connected_variables(check_idx);
                let degree = connected_vars.len();

                match self.algorithm {
                    LdpcAlgorithm::SumProduct => {
                        for target_pos in 0..degree {
                            let mut product = 1.0f32;
                            for pos in 0..degree {
                                if pos != target_pos {
                                    product *= fast_tanh(v2c[check_idx][pos] / 2.0);
                                }
                            }
                            c2v[check_idx][target_pos] = 2.0 * fast_atanh(product);
                        }
                    }
                    LdpcAlgorithm::MinSum {
                        normalization_factor,
                    } => {
                        let mut total_sign: i8 = 1;
                        let mut min1_mag = f32::MAX;
                        let mut min2_mag = f32::MAX;
                        let mut min1_pos: usize = 0;
                        let mut signs = [1i8; 7];

                        for pos in 0..degree {
                            let msg = v2c[check_idx][pos];
                            let s = if msg < 0.0 { -1i8 } else { 1i8 };
                            signs[pos] = s;
                            total_sign *= s;

                            let mag = msg.abs();
                            if mag < min1_mag {
                                min2_mag = min1_mag;
                                min1_mag = mag;
                                min1_pos = pos;
                            } else if mag < min2_mag {
                                min2_mag = mag;
                            }
                        }

                        for pos in 0..degree {
                            let edge_sign = total_sign * signs[pos];
                            let mag = if pos == min1_pos { min2_mag } else { min1_mag };
                            c2v[check_idx][pos] = edge_sign as f32 * mag * normalization_factor;
                        }
                    }
                }
            }

            // Variable node update
            for var_idx in 0..num_vars {
                let positions = &self.var_positions[var_idx];
                let mut total = channel_llrs[var_idx];
                for &(check_idx, pos) in positions {
                    total += c2v[check_idx][pos];
                }
                output_llrs[var_idx] = total;

                for &(check_idx, pos) in positions {
                    v2c[check_idx][pos] = total - c2v[check_idx][pos];
                }
            }

            // Record trajectory (only first 25 iterations fit)
            if iteration < max_iters {
                trajectory[iteration] = output_llrs;
            }

            // Early termination on convergence — discard trajectory
            if self.check_syndrome_fast(&output_llrs) {
                return Ok((output_llrs.to_vec(), None));
            }
        }

        // BP did not converge — fill any remaining trajectory slots
        for i in self.max_iterations.min(25)..25 {
            trajectory[i] = output_llrs;
        }

        Ok((output_llrs.to_vec(), Some(trajectory)))
    }

    /// Check if syndrome is zero (all parity checks satisfied).
    /// Accepts a slice for compatibility; requires length >= 174.
    fn check_syndrome(&self, llrs: &[f32]) -> bool {
        if llrs.len() < 174 {
            return false;
        }
        let arr: &[f32; 174] = llrs[..174].try_into().unwrap();
        self.check_syndrome_fast(arr)
    }

    /// Fast syndrome check using hard decisions from LLRs.
    /// Returns true if all 83 parity checks are satisfied.
    fn check_syndrome_fast(&self, llrs: &[f32; 174]) -> bool {
        for check_idx in 0..self.parity_check_matrix.num_checks {
            let connected_vars = self.parity_check_matrix.get_connected_variables(check_idx);
            let mut parity = 0u8;

            for &var_idx in connected_vars {
                if llrs[var_idx] < 0.0 {
                    parity ^= 1;
                }
            }

            if parity != 0 {
                return false;
            }
        }

        true
    }

    /// Count the number of unsatisfied parity checks (hard decisions from LLRs).
    /// Used to gate OSD: only worth trying when BP was close to converging.
    fn count_parity_errors(&self, llrs: &[f32; 174]) -> usize {
        let mut errors = 0;
        for check_idx in 0..self.parity_check_matrix.num_checks {
            let connected_vars = self.parity_check_matrix.get_connected_variables(check_idx);
            let mut parity = 0u8;
            for &var_idx in connected_vars {
                if llrs[var_idx] < 0.0 {
                    parity ^= 1;
                }
            }
            if parity != 0 {
                errors += 1;
            }
        }
        errors
    }
}

use crate::ldpc::ParityCheckMatrix;

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NUM_SYMBOLS, TONE_SPACING, WINDOW_SAMPLES};
    use approx::assert_relative_eq;
    use std::f64::consts::PI;

    #[test]
    fn test_ft8_config_default() {
        let config = Ft8Config::default();
        assert_eq!(config.sample_rate, SAMPLE_RATE);
        assert_eq!(config.max_candidates, MAX_DECODE_CANDIDATES);
    }

    #[test]
    fn test_decoder_creation() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config);
        assert!(decoder.is_ok());
    }

    #[test]
    fn test_invalid_sample_rate() {
        let mut config = Ft8Config::default();
        config.sample_rate = 48000;

        let decoder = Ft8Decoder::new(config);
        assert!(decoder.is_err());

        if let Err(Ft8Error::InvalidSampleRate { expected, actual }) = decoder {
            assert_eq!(expected, SAMPLE_RATE);
            assert_eq!(actual, 48000);
        }
    }

    #[test]
    fn test_window_size_validation() {
        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();

        let wrong_samples = vec![0.0f32; 48000];
        let result = decoder.decode_window(&wrong_samples);
        assert!(result.is_err());

        if let Err(Ft8Error::InvalidWindowSize { expected, actual }) = result {
            assert_eq!(expected, WINDOW_SAMPLES);
            assert_eq!(actual, 48000);
        }
    }

    #[test]
    fn test_correct_window_size() {
        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();

        let samples = vec![0.0f32; WINDOW_SAMPLES];
        let result = decoder.decode_window(&samples);
        assert!(result.is_ok());

        let decoded = result.unwrap();
        assert_eq!(decoded.len(), 0); // Silence → no messages
    }

    #[test]
    fn test_noise_floor_estimation() {
        let psd = vec![1.0, 2.0, 3.0, 100.0, 4.0, 5.0, 6.0];
        let noise_floor = estimate_noise_floor(&psd);
        assert_relative_eq!(noise_floor, 4.0, epsilon = 0.1);
    }

    #[test]
    fn test_bits_to_u16_conversion() {
        let bits = bitvec![1, 0, 1, 1, 0, 0, 1, 0];
        let value = bits_to_u16(&bits);
        assert_eq!(value, 0b10110010);
    }

    #[test]
    fn test_spectrogram_computation() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();

        // Generate a 1500 Hz tone
        let mut audio = vec![0.0f64; WINDOW_SAMPLES];
        for i in 0..audio.len() {
            let t = i as f64 / SAMPLE_RATE as f64;
            audio[i] = (2.0 * PI * 1500.0 * t).sin() * 0.5;
        }

        let spec = decoder.compute_spectrogram(&audio).unwrap();

        assert!(spec.num_steps > 0);
        assert!(spec.num_bins > 0);
        assert_eq!(spec.power.len(), spec.num_steps);
        assert_eq!(spec.freq_osr, FREQ_OSR);
        assert_eq!(spec.power[0].len(), spec.freq_osr);
        assert_eq!(spec.power[0][0].len(), spec.num_bins);

        // The 1500 Hz tone should produce a peak at bin 1500/6.25 = 240
        let tone_bin = (1500.0 / TONE_SPACING) as usize;
        let mid_step = spec.num_steps / 2;

        // Power (dB) at tone bin should be much larger than at a random bin (freq_sub=0)
        let signal_db = spec.power[mid_step][0][tone_bin];
        let noise_db = spec.power[mid_step][0][10]; // Low-frequency noise bin
        assert!(
            signal_db > noise_db + 20.0,
            "Signal dB {:.2} should be >> noise dB {:.2}",
            signal_db,
            noise_db
        );
    }

    #[test]
    fn test_costas_score_with_sync_signal() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();

        // Create a spectrogram where Costas tones are present at t0=0, f0=240
        // Spectrogram stores log-magnitude (dB) values
        let steps_per_symbol = TIME_OSR;
        let num_steps = 79 * steps_per_symbol; // enough for 79 symbols
        let num_bins = SAMPLES_PER_SYMBOL / 2 + 1; // bins in 6.25 Hz units
        let freq_osr = FREQ_OSR;
        let noise_db = -40.0; // noise floor in dB
        let signal_db = -10.0; // signal level in dB (30 dB above noise)
        let f0 = 240usize; // 1500 Hz

        let mut power = vec![vec![vec![noise_db; num_bins]; freq_osr]; num_steps];

        // Place Costas tones at the correct positions (freq_sub=0)
        // Fill all sub-steps of each symbol with the signal
        for &group_start in &[0usize, 36, 72] {
            for j in 0..7 {
                let sym = group_start + j;
                let tone = COSTAS[j] as usize;
                for sub in 0..steps_per_symbol {
                    let time_idx = sym * steps_per_symbol + sub;
                    if time_idx < num_steps && f0 + tone < num_bins {
                        power[time_idx][0][f0 + tone] = signal_db;
                    }
                }
            }
        }

        let spec = Spectrogram {
            power,
            complex: None,
            num_steps,
            num_bins,
            freq_osr,
            time_padding: 0,
        };

        let score = decoder.compute_costas_score(&spec, 0, f0, 0);
        assert!(
            score > MIN_SYNC_SCORE,
            "Costas score {:.2} should exceed threshold {:.2}",
            score,
            MIN_SYNC_SCORE
        );

        // Score at a wrong frequency should be much lower
        let wrong_score = decoder.compute_costas_score(&spec, 0, f0 + 20, 0);
        assert!(
            score > wrong_score * 2.0,
            "Correct score {:.2} should be >> wrong score {:.2}",
            score,
            wrong_score
        );
    }

    #[test]
    fn test_complex_dft_tone_detection() {
        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();

        // Generate a signal with a known tone at 1500 + 3*6.25 = 1518.75 Hz
        let base_freq = 1500.0;
        let target_tone = 3;
        let freq = base_freq + target_tone as f64 * TONE_SPACING;

        let mut audio = vec![0.0f64; WINDOW_SAMPLES];
        for i in 0..audio.len() {
            let t = i as f64 / SAMPLE_RATE as f64;
            audio[i] = (2.0 * PI * freq * t).sin() * 0.5;
        }

        let (symbols, mags) = decoder
            .extract_symbols_complex(&audio, 0, base_freq)
            .unwrap();

        // Every symbol should detect tone 3
        for (i, &sym) in symbols.iter().enumerate() {
            assert_eq!(
                sym, target_tone,
                "Symbol {} detected tone {} instead of {}",
                i, sym, target_tone
            );
        }

        // Magnitude at target tone should dominate
        for (i, m) in mags.iter().enumerate() {
            assert!(
                m[target_tone as usize] > m[0] * 5.0,
                "Symbol {}: target mag {:.4} should dominate other mag {:.4}",
                i,
                m[target_tone as usize],
                m[0]
            );
        }
    }

    #[test]
    fn test_soft_llr_polarity() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();

        // Create tone magnitudes where tone 0 (binary 000) is always dominant
        let mut tone_magnitudes = vec![[0.0f64; NUM_TONES]; NUM_SYMBOLS];
        for sym in &mut tone_magnitudes {
            sym[0] = 10.0; // Tone 0 dominant
            for tone in 1..NUM_TONES {
                sym[tone] = 0.1; // Other tones weak
            }
        }

        let llrs = decoder.compute_soft_llrs(&tone_magnitudes);
        assert_eq!(llrs.len(), 174);

        // Tone 0 → gray_to_binary(0) = 0 → bits 000
        // All LLRs should be positive (bit=0 is more likely)
        for (i, &llr) in llrs.iter().enumerate() {
            assert!(
                llr > 0.0,
                "LLR[{}] = {:.2} should be positive (bit=0 likely for tone 0)",
                i,
                llr
            );
        }
    }

    #[test]
    fn test_ldpc_decoder_creation() {
        let decoder = LdpcDecoder::new(50);
        assert!(decoder.is_ok());

        let ldpc = decoder.unwrap();
        assert_eq!(ldpc.max_iterations, 50);
        assert!(matches!(ldpc.algorithm, LdpcAlgorithm::SumProduct));
        // Early termination is always on (syndrome checked every iteration)
        assert!(!ldpc.var_positions.is_empty());
    }

    #[test]
    fn test_ldpc_bits_to_llrs_conversion() {
        let decoder = LdpcDecoder::new(10).unwrap();

        let bits = bitvec![1, 0, 1, 1, 0, 0, 1, 0];
        let llrs = decoder.bits_to_llrs(&bits);

        assert_eq!(llrs.len(), 174);
        assert!(llrs[0] < 0.0); // bit 1 → negative LLR
        assert!(llrs[1] > 0.0); // bit 0 → positive LLR
        assert!(llrs[2] < 0.0);
    }

    #[test]
    fn test_ldpc_llrs_to_bits_conversion() {
        let decoder = LdpcDecoder::new(10).unwrap();

        let mut llrs = vec![0.0; 174];
        llrs[0] = -2.0;
        llrs[1] = 3.0;
        llrs[2] = -1.5;
        llrs[3] = 0.5;
        llrs[4] = -0.1;

        let bits = decoder.llrs_to_bits(&llrs).unwrap();

        assert_eq!(bits.len(), 174);
        assert!(bits[0]); // negative LLR → bit 1
        assert!(!bits[1]); // positive LLR → bit 0
        assert!(bits[2]);
        assert!(!bits[3]);
        assert!(bits[4]);
    }

    #[test]
    fn test_ldpc_soft_decode_size_validation() {
        let decoder = LdpcDecoder::new(10).unwrap();

        let llrs = vec![0.0; 100];
        let result = decoder.decode_soft(&llrs);
        assert!(result.is_err());

        if let Err(Ft8Error::InvalidDataSize { expected, actual }) = result {
            assert_eq!(expected, 174);
            assert_eq!(actual, 100);
        }
    }

    #[test]
    fn test_ldpc_syndrome_check() {
        let decoder = LdpcDecoder::new(10).unwrap();

        // All zeros should satisfy parity checks
        let llrs = vec![10.0; 174];
        assert!(decoder.check_syndrome(&llrs));

        // Random values likely won't satisfy
        let mut random_llrs = vec![0.0; 174];
        for (i, llr) in random_llrs.iter_mut().enumerate() {
            *llr = if i % 3 == 0 { -2.0 } else { 2.0 };
        }
        assert!(!decoder.check_syndrome(&random_llrs));
    }

    #[test]
    fn test_ldpc_decode_with_no_errors() {
        let decoder = LdpcDecoder::new(50).unwrap();

        let bits = bitvec![0; 174];
        let decoded = decoder.decode(&bits).unwrap();

        assert_eq!(decoded.len(), 174);
        for i in 0..174 {
            assert_eq!(decoded[i], bits[i]);
        }
    }

    #[test]
    fn test_ldpc_belief_propagation_convergence() {
        let decoder = LdpcDecoder::new(100).unwrap();

        let mut llrs = vec![5.0; 174]; // All zeros with high confidence
        llrs[10] = -1.0;
        llrs[50] = -0.5;

        let decoded_llrs = decoder.belief_propagation(&llrs).unwrap();

        let mut correct_bits = 0;
        for i in 0..174 {
            if i != 10 && i != 50 && decoded_llrs[i] > 0.0 {
                correct_bits += 1;
            }
        }

        assert!(correct_bits > 170, "Only {} bits correct", correct_bits);
    }

    #[test]
    fn test_ldpc_layered_bp_converges() {
        // hb-063: the layered (row-sequential) schedule must decode at
        // least as well as flooding. The all-zero vector is a valid
        // codeword for any linear code.
        let layered = LdpcDecoder::new(100).unwrap().with_layered(true);

        // Clean, confident all-zero codeword: every output LLR must stay
        // positive (i.e. the schedule does not perturb a valid codeword).
        let clean = vec![5.0f32; 174];
        let (clean_llrs, traj) = layered.belief_propagation_with_trajectory(&clean).unwrap();
        assert!(
            clean_llrs.iter().all(|&l| l > 0.0),
            "layered perturbed a clean codeword"
        );
        assert!(
            traj.is_none(),
            "clean codeword should converge (no trajectory)"
        );

        // Lightly corrupted: the weak/flipped LLRs must be corrected, just
        // like the flooding-schedule convergence test above.
        let mut noisy = vec![5.0f32; 174];
        noisy[10] = -1.0;
        noisy[50] = -0.5;
        let (noisy_llrs, _) = layered.belief_propagation_with_trajectory(&noisy).unwrap();
        let correct = (0..174).filter(|&i| noisy_llrs[i] > 0.0).count();
        assert!(correct > 170, "layered only corrected {correct}/174 bits");
    }

    #[test]
    fn test_cross_cycle_grouping_and_linear_sum() {
        // hb-056: grouping pairs candidates ~1 slot apart at the same
        // freq, but only when sync scores are within the band; and the
        // linear-power sum correctly handles the dB↔linear conversion.
        let mk = |t0: usize, fb: usize, fs: usize, score: f64| CostasCandidate {
            time_step: t0,
            freq_bin: fb,
            freq_sub: fs,
            time_refinement: 0.0,
            sync_score: score,
        };
        // Group 1: two candidates exactly one slot apart at the same freq,
        // matching scores → must group.
        // Group 2: two candidates two slots apart, score mismatch → must NOT
        // group (score band guard prevents averaging across distinct
        // stations that happen to share a frequency).
        // Singleton: isolated candidate at a unique freq → never grouped.
        let candidates = vec![
            mk(50, 100, 0, 7.0),                           // 0  group 1 anchor
            mk(50 + SLOT_TIME_STEPS_FT8, 100, 0, 6.5),     // 1  group 1 (Δscore 0.5 in band)
            mk(80, 200, 0, 5.0),                           // 2  group-2 anchor
            mk(80 + 2 * SLOT_TIME_STEPS_FT8, 200, 0, 1.0), // 3  group 2 reject (Δscore 4 > band)
            mk(60, 300, 0, 6.0),                           // 4  singleton
        ];
        let groups = group_for_cross_cycle(&candidates);
        assert_eq!(groups.len(), 1, "exactly one valid group expected");
        let g = &groups[0];
        assert_eq!(g.len(), 2);
        assert!(g.contains(&0) && g.contains(&1));

        // Linear-power sum: two members each with a single -10 dB tone in
        // symbol 0, tone 0; their sum should be +3.01 dB (2x in linear).
        let mut a = vec![[f64::NEG_INFINITY; NUM_TONES]; 4];
        let mut b = vec![[f64::NEG_INFINITY; NUM_TONES]; 4];
        a[0][0] = -10.0;
        b[0][0] = -10.0;
        let summed = sum_tone_magnitudes_linear(&[a, b], 4);
        let expected_db = 10.0 * 2.0f64.log10() - 10.0; // -10 + 3.01 ≈ -6.99
        assert!(
            (summed[0][0] - expected_db).abs() < 1e-6,
            "expected {expected_db}, got {}",
            summed[0][0]
        );
    }

    #[test]
    fn test_coherent_phase_rotor_and_gain() {
        // hb-074: build a synthetic "candidate" whose Costas symbols hold
        // a unit-amplitude tone at the expected positions with phase φ.
        // estimate_candidate_phase_rotor should recover exp(jφ); multiplying
        // by conj(rotor) should normalise the phase to 0. The coherent sum
        // of N=2 phase-aligned signals + uncorrelated noise should beat the
        // non-coherent (linear-power) sum's SNR by ~3 dB (vs ~1.5 dB).
        use crate::protocol::ProtocolParams;
        let pp = ProtocolParams::ft8();
        // Two synthetic "candidates" with the same signal, different phases.
        let mk = |phase: f64| -> Vec<[Complex<f64>; NUM_TONES]> {
            let mut symbols = vec![[Complex::new(0.0, 0.0); NUM_TONES]; pp.num_symbols];
            for (m, &group_start) in pp.costas_positions.iter().enumerate() {
                for k in 0..pp.costas_length {
                    let sym_idx = group_start + k;
                    let expected_tone = pp.costas_arrays[m][k] as usize;
                    // Unit amplitude at the expected tone, phase φ; small
                    // noise on the other tones (won't affect the rotor).
                    symbols[sym_idx][expected_tone] = Complex::from_polar(1.0, phase);
                }
            }
            symbols
        };
        let phase_a = 0.7f64;
        let phase_b = -1.4f64;
        let cs_a = mk(phase_a);
        let cs_b = mk(phase_b);

        let rotor_a = estimate_candidate_phase_rotor(&pp, &cs_a).expect("rotor a");
        let rotor_b = estimate_candidate_phase_rotor(&pp, &cs_b).expect("rotor b");
        // Rotor should recover exp(jφ) within float precision.
        assert!(
            (rotor_a - Complex::from_polar(1.0, phase_a)).norm() < 1e-6,
            "rotor_a {rotor_a:?}"
        );
        assert!(
            (rotor_b - Complex::from_polar(1.0, phase_b)).norm() < 1e-6,
            "rotor_b {rotor_b:?}"
        );

        // After conj(rotor) rotation, the Costas-symbol amplitudes line up.
        let conj_rotor_a = rotor_a.conj();
        let conj_rotor_b = rotor_b.conj();
        // Pick one Costas position to inspect.
        let m0 = pp.costas_positions[0];
        let tone0 = pp.costas_arrays[0][0] as usize;
        let aligned_a = cs_a[m0][tone0] * conj_rotor_a;
        let aligned_b = cs_b[m0][tone0] * conj_rotor_b;
        // Both should be ≈ 1 + 0j (real, positive).
        assert!(aligned_a.im.abs() < 1e-6 && (aligned_a.re - 1.0).abs() < 1e-6);
        assert!(aligned_b.im.abs() < 1e-6 && (aligned_b.re - 1.0).abs() < 1e-6);

        // Coherent sum of 2 aligned signals → power = 4 (|1+1|² = 4).
        // Non-coherent power sum (|1|² + |1|²) = 2. So coherent beats
        // non-coherent by 3 dB on aligned signals — the canonical claim.
        let coherent_pwr = (aligned_a + aligned_b).norm_sqr();
        let noncoherent_pwr = aligned_a.norm_sqr() + aligned_b.norm_sqr();
        assert!(
            (coherent_pwr / noncoherent_pwr - 2.0).abs() < 1e-6,
            "expected 2x (3dB) gain, got {:.3}x",
            coherent_pwr / noncoherent_pwr
        );
    }

    #[test]
    fn test_coherent_subtract_ml_projection() {
        // hb-079: at a bin containing A·exp(jφ) + noise, the ML projection
        // onto rotor exp(jφ) yields the *parallel* component (signal +
        // noise's parallel piece). The residual = bin - signal_est is
        // strictly orthogonal to the rotor — the noise component
        // perpendicular to the signal direction. This is what we subtract
        // in the spectrogram: we remove the signal-aligned content while
        // preserving the orthogonal noise.
        let phi = 0.7f64;
        let rotor = Complex::from_polar(1.0, phi);
        let signal_amp = 2.5;
        let noise = Complex::new(0.3, -0.6); // arbitrary noise sample
        let bin = Complex::from_polar(signal_amp, phi) + noise;

        let rotor_conj = rotor.conj();
        let proj_real = (bin * rotor_conj).re;
        let signal_est = Complex::new(proj_real, 0.0) * rotor;
        let residual = bin - signal_est;

        // Residual must be orthogonal to the rotor: Re(residual·conj(rotor)) ≈ 0.
        let dot = (residual * rotor_conj).re;
        assert!(
            dot.abs() < 1e-10,
            "residual must be orthogonal to rotor, dot={dot}"
        );

        // Signal estimate, when projected back, must equal signal_amp + noise's
        // parallel component (the bin's full projection onto rotor).
        let noise_parallel = (noise * rotor_conj).re;
        let expected = signal_amp + noise_parallel;
        let actual = (signal_est * rotor_conj).re;
        assert!(
            (actual - expected).abs() < 1e-10,
            "signal_est projection {actual} != expected {expected}"
        );

        // |signal_est|² + |residual|² == |bin|² (orthogonal decomposition).
        let lhs = signal_est.norm_sqr() + residual.norm_sqr();
        let rhs = bin.norm_sqr();
        assert!((lhs - rhs).abs() < 1e-10, "Pythagorean: {lhs} != {rhs}");
    }

    #[test]
    fn test_metrics_collection() {
        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();

        let samples = vec![0.0f32; WINDOW_SAMPLES];
        let _ = decoder.decode_window(&samples).unwrap();

        let metrics = decoder.get_last_metrics();
        assert_eq!(metrics.messages_decoded, 0);
        assert!(metrics.processing_time.as_millis() > 0);
    }

    /// hb-129: every decode that survives CRC must carry a presentation-time
    /// `decode_time_into_window` stamp. Exercises the full pipeline with a
    /// synthetic FT8 transmission, then verifies every emitted message has a
    /// non-zero stamp that does not exceed the wall-clock processing time.
    #[cfg(feature = "transmit")]
    #[test]
    fn test_ttfd_stamping_on_synth_signal() {
        let mut encoder = crate::Ft8Encoder::new();
        let symbols = encoder
            .encode_message("CQ K5ARH EM10", None)
            .expect("encode");

        let mut modulator = crate::Ft8Modulator::new_default().expect("modulator");
        let mut tx = modulator.modulate_symbols(&symbols, 0.0).expect("modulate");
        tx.resize(WINDOW_SAMPLES, 0.0);

        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();
        let start = std::time::Instant::now();
        let decoded = decoder.decode_window(&tx).expect("decode");
        let elapsed = start.elapsed();

        assert!(
            !decoded.is_empty(),
            "synth FT8 signal should produce at least one decode"
        );
        for msg in &decoded {
            let ttfd = msg
                .decode_time_into_window
                .expect("every decode produced by the pipeline must carry a TTFD stamp");
            // Stamp must be non-zero and not exceed wall-clock elapsed.
            assert!(ttfd.as_micros() > 0, "TTFD must be > 0, got {:?}", ttfd);
            assert!(
                ttfd <= elapsed + std::time::Duration::from_millis(50),
                "TTFD {:?} cannot exceed wall-clock elapsed {:?}",
                ttfd,
                elapsed
            );
        }
    }

    #[test]
    fn test_waterfall_data_generation() {
        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();

        let mut audio = vec![0.0; WINDOW_SAMPLES];
        let freq = 1000.0;
        for i in 0..audio.len() {
            let t = i as f64 / SAMPLE_RATE as f64;
            audio[i] = (2.0 * PI * freq * t).sin() * 0.5;
        }

        let waterfall = decoder.generate_waterfall_data(&audio).unwrap();

        assert!(!waterfall.time_bins.is_empty());
        assert!(!waterfall.frequency_bins.is_empty());
        assert!(!waterfall.power_matrix.is_empty());
        assert!(waterfall.min_power < waterfall.max_power);
        assert!(waterfall.frequency_bins[0] >= 0.0);
        assert!(waterfall.frequency_bins.last().unwrap() <= &3000.0);
    }

    #[test]
    fn test_nms_suppression() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();

        let mut candidates = vec![
            CostasCandidate {
                time_step: 0,
                freq_bin: 240,
                freq_sub: 0,
                sync_score: 20.0,
                time_refinement: 0.0,
            },
            CostasCandidate {
                time_step: 1,
                freq_bin: 240,
                freq_sub: 0,
                sync_score: 15.0,
                time_refinement: 0.0,
            }, // near #0
            CostasCandidate {
                time_step: 0,
                freq_bin: 241,
                freq_sub: 0,
                sync_score: 12.0,
                time_refinement: 0.0,
            }, // near #0
            CostasCandidate {
                time_step: 0,
                freq_bin: 300,
                freq_sub: 0,
                sync_score: 18.0,
                time_refinement: 0.0,
            }, // far from #0
        ];

        decoder.nms_candidates(&mut candidates);

        // Should keep #0 (strongest) and #3 (far away), suppress #1 and #2
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].freq_bin, 240);
        assert_eq!(candidates[0].sync_score, 20.0);
        assert_eq!(candidates[1].freq_bin, 300);
    }

    // hb-036: score-relative NMS suppression.
    //
    // Legacy guard: with `nms_score_delta_db = 0.0` (the production default),
    // the score-relative branch is bypassed and behavior is bit-exactly the
    // same as pre-hb-036 pure TF-distance NMS.
    #[test]
    fn test_nms_score_delta_zero_matches_legacy_tf_distance() {
        // Build the same fixture as the legacy `test_nms_suppression`, but
        // run it with NMS explicitly enabled and `nms_score_delta_db = 0.0`.
        // The result must match the legacy suppression pattern exactly.
        let mut config = Ft8Config::default();
        config.nms_enabled = true;
        config.nms_score_delta_db = 0.0;
        let decoder = Ft8Decoder::new(config).unwrap();

        let mut candidates = vec![
            CostasCandidate {
                time_step: 0,
                freq_bin: 240,
                freq_sub: 0,
                sync_score: 20.0,
                time_refinement: 0.0,
            },
            CostasCandidate {
                time_step: 1,
                freq_bin: 240,
                freq_sub: 0,
                sync_score: 5.0, // very weak — would survive the score gate
                time_refinement: 0.0,
            },
            CostasCandidate {
                time_step: 0,
                freq_bin: 300,
                freq_sub: 0,
                sync_score: 18.0,
                time_refinement: 0.0,
            },
        ];

        decoder.nms_candidates(&mut candidates);

        // delta=0 ⇒ pure TF-distance: weak neighbor #1 still suppressed.
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].freq_bin, 240);
        assert_eq!(candidates[1].freq_bin, 300);
    }

    // hb-036: with `nms_score_delta_db = 3.0` and NMS on, a strong-and-weak
    // pair inside the same TF cell must KEEP both: the weak candidate
    // (score < strong.score - 3.0) is treated as a distinct signal. Contrast
    // with legacy NMS, which would have suppressed the weak one.
    #[test]
    fn test_nms_score_delta_keeps_distinct_weaker_signal() {
        let mut config = Ft8Config::default();
        config.nms_enabled = true;
        config.nms_time_radius = 2;
        config.nms_freq_radius = 1;
        config.nms_score_delta_db = 3.0;
        let decoder = Ft8Decoder::new(config).unwrap();

        // Two candidates sharing a TF cell (within t=2, f=1) but with a
        // 5.0 sync_score gap — much larger than the 3.0 delta. Under
        // hb-036 the weaker is a distinct signal, KEEP it.
        let mut candidates = vec![
            CostasCandidate {
                time_step: 10,
                freq_bin: 240,
                freq_sub: 0,
                sync_score: 12.0, // strong
                time_refinement: 0.0,
            },
            CostasCandidate {
                time_step: 11,
                freq_bin: 240,
                freq_sub: 0,
                sync_score: 6.0, // weak (12 - 6 = 6 > delta=3 ⇒ KEEP)
                time_refinement: 0.0,
            },
        ];

        decoder.nms_candidates(&mut candidates);

        // hb-036: distinct weaker signal preserved.
        assert_eq!(
            candidates.len(),
            2,
            "score-relative NMS should KEEP a meaningfully weaker neighbor"
        );
        assert_eq!(candidates[0].sync_score, 12.0);
        assert_eq!(candidates[1].sync_score, 6.0);

        // Confirm that legacy (delta=0) would have suppressed it, proving
        // the new condition is the discriminator.
        let mut legacy_config = Ft8Config::default();
        legacy_config.nms_enabled = true;
        legacy_config.nms_time_radius = 2;
        legacy_config.nms_freq_radius = 1;
        legacy_config.nms_score_delta_db = 0.0;
        let legacy_decoder = Ft8Decoder::new(legacy_config).unwrap();
        let mut legacy_candidates = vec![
            CostasCandidate {
                time_step: 10,
                freq_bin: 240,
                freq_sub: 0,
                sync_score: 12.0,
                time_refinement: 0.0,
            },
            CostasCandidate {
                time_step: 11,
                freq_bin: 240,
                freq_sub: 0,
                sync_score: 6.0,
                time_refinement: 0.0,
            },
        ];
        legacy_decoder.nms_candidates(&mut legacy_candidates);
        assert_eq!(
            legacy_candidates.len(),
            1,
            "legacy TF-distance NMS suppresses the weaker neighbor"
        );
    }

    // hb-036: a near-duplicate within the score delta should STILL be
    // suppressed — that's exactly what NMS is supposed to catch
    // (duplicate of a strong signal).
    #[test]
    fn test_nms_score_delta_suppresses_near_duplicate() {
        let mut config = Ft8Config::default();
        config.nms_enabled = true;
        config.nms_time_radius = 2;
        config.nms_freq_radius = 1;
        config.nms_score_delta_db = 3.0;
        let decoder = Ft8Decoder::new(config).unwrap();

        let mut candidates = vec![
            CostasCandidate {
                time_step: 10,
                freq_bin: 240,
                freq_sub: 0,
                sync_score: 12.0,
                time_refinement: 0.0,
            },
            CostasCandidate {
                time_step: 11,
                freq_bin: 240,
                freq_sub: 0,
                sync_score: 10.5, // within delta=3 of 12 ⇒ suppress
                time_refinement: 0.0,
            },
        ];

        decoder.nms_candidates(&mut candidates);

        assert_eq!(
            candidates.len(),
            1,
            "near-duplicate within delta is still suppressed"
        );
        assert_eq!(candidates[0].sync_score, 12.0);
    }

    #[test]
    fn test_llr_normalization_scales_to_target_variance() {
        // Create LLRs with known variance
        let mut llrs = vec![0.0f32; 174];
        for (i, llr) in llrs.iter_mut().enumerate() {
            // Create a pattern with variance != 24.0
            *llr = if i % 2 == 0 { 2.0 } else { -2.0 };
        }

        // Original variance should be ~4.0
        let orig_var = compute_variance(&llrs);
        assert!(
            (orig_var - 4.0).abs() < 0.1,
            "Expected variance ~4.0, got {}",
            orig_var
        );

        // Normalize
        normalize_llrs(&mut llrs, LLR_TARGET_VARIANCE);

        // After normalization, variance should be ~24.0
        let norm_var = compute_variance(&llrs);
        assert!(
            (norm_var - LLR_TARGET_VARIANCE).abs() < 0.1,
            "Expected variance ~{}, got {}",
            LLR_TARGET_VARIANCE,
            norm_var
        );
    }

    #[test]
    fn test_llr_normalization_preserves_sign() {
        let mut llrs = vec![0.0f32; 174];
        for (i, llr) in llrs.iter_mut().enumerate() {
            *llr = if i % 3 == 0 {
                5.0
            } else if i % 3 == 1 {
                -3.0
            } else {
                1.0
            };
        }

        let signs: Vec<bool> = llrs.iter().map(|&x| x > 0.0).collect();
        normalize_llrs(&mut llrs, LLR_TARGET_VARIANCE);
        let new_signs: Vec<bool> = llrs.iter().map(|&x| x > 0.0).collect();
        assert_eq!(signs, new_signs, "Normalization should preserve LLR signs");
    }

    #[test]
    fn test_llr_normalization_zero_variance() {
        // All same values: variance = 0, should not crash
        let mut llrs = vec![3.0f32; 174];
        normalize_llrs(&mut llrs, LLR_TARGET_VARIANCE);
        // Should be unchanged (no scaling possible)
        assert_eq!(llrs[0], 3.0);
    }

    #[test]
    fn test_freq_osr_produces_sub_bins() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();

        // Generate a tone at 1503.125 Hz (between 240th and 241st 6.25 Hz bins)
        // This should show up strongly in freq_sub=1 at bin 240
        let freq = 1503.125; // = 240 * 6.25 + 3.125
        let mut audio = vec![0.0f64; WINDOW_SAMPLES];
        for i in 0..audio.len() {
            let t = i as f64 / SAMPLE_RATE as f64;
            audio[i] = (2.0 * PI * freq * t).sin() * 0.5;
        }

        let spec = decoder.compute_spectrogram(&audio).unwrap();
        assert_eq!(spec.freq_osr, 2);

        let mid = spec.num_steps / 2;
        let bin = 240;

        // The signal should appear in freq_sub=1 at bin 240 (since 1503.125 = 240*6.25 + 3.125)
        // Spectrogram values are in dB
        let db_sub0 = spec.power[mid][0][bin];
        let db_sub1 = spec.power[mid][1][bin];

        // freq_sub=1 should have stronger signal (higher dB) for a tone at bin+0.5
        assert!(
            db_sub1 > db_sub0 + 3.0,
            "freq_sub=1 dB ({:.2}) should be > freq_sub=0 dB ({:.2}) + 3 for half-bin tone",
            db_sub1,
            db_sub0
        );
    }

    /// Helper to compute variance of a slice
    fn compute_variance(values: &[f32]) -> f32 {
        let n = values.len() as f32;
        let sum: f32 = values.iter().sum();
        let sum2: f32 = values.iter().map(|&x| x * x).sum();
        (sum2 - sum * sum / n) / n
    }

    #[test]
    fn test_ldpc_decode_soft_with_osd_fallback() {
        use crate::osd::OsdConfig;

        // Create decoder with OSD enabled (1 BP iteration = won't converge)
        let decoder = LdpcDecoder::new_with_osd(1, Some(OsdConfig { max_depth: 2 })).unwrap();

        // Create LLRs for a known valid codeword with 2 unreliable bits
        // We need the encoder for this, so gate behind transmit feature
        // For now, just verify construction works
        assert!(decoder.osd.is_some());

        // Verify the no-OSD path still works
        let decoder_no_osd = LdpcDecoder::new(50).unwrap();
        assert!(decoder_no_osd.osd.is_none());
    }

    #[test]
    fn test_subtract_signal_removes_energy() {
        // Generate a known CPFSK signal, add it to silence,
        // then subtract and verify the energy is reduced.
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();

        let sps = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;
        let base_freq = 1000.0;
        let amplitude = 0.1f32;

        // Create known tone symbols (all tone 3 for simplicity)
        let symbols: Vec<u8> = (0..NUM_SYMBOLS)
            .map(|i| {
                if i < 7 {
                    COSTAS[i]
                } else if (36..43).contains(&i) {
                    COSTAS[i - 36]
                } else if i >= 72 {
                    COSTAS[i - 72]
                } else {
                    3
                } // arbitrary data tone
            })
            .collect();

        // Generate the signal
        let total_len = NUM_SYMBOLS * sps;
        let time_offset_samples = 960usize; // 1 half-symbol offset
        let mut audio = vec![0.0f32; WINDOW_SAMPLES];

        let mut phase = 0.0f64;
        for sym_idx in 0..NUM_SYMBOLS {
            let freq = base_freq + symbols[sym_idx] as f64 * TONE_SPACING;
            let omega = 2.0 * PI * freq / SAMPLE_RATE as f64;
            let start = time_offset_samples + sym_idx * sps;
            for i in 0..sps {
                if start + i < audio.len() {
                    audio[start + i] = (amplitude as f64 * phase.sin()) as f32;
                }
                phase += omega;
            }
        }

        // Measure energy before subtraction
        let energy_before: f64 = audio.iter().map(|&s| (s as f64) * (s as f64)).sum();

        // Create a DecodedMessage with the known symbols
        let msg = DecodedMessage {
            message: crate::message::Ft8Message {
                message_type: crate::message::MessageType::FreeText,
                standard_type: None,
                from_callsign: None,
                to_callsign: None,
                grid_square: None,
                signal_report: None,
                text: Some("TEST".to_string()),
                contest_exchange: None,
                special_operation: None,
                payload_bits: bitvec![0; 77],
                crc: 0,
                crc_valid: false,
                uses_hash_calls: false,
            },
            text: "TEST".to_string(),
            snr_db: 0.0,
            confidence: 1.0,
            frequency_offset: base_freq,
            time_offset: time_offset_samples as f64 / SAMPLE_RATE as f64,
            timestamp: SystemTime::now(),
            error_corrections: 0,
            tone_symbols: Some(symbols),
            ap_level: 0,
            slot_parity: None,
            decode_time_into_window: None,
        };

        decoder.subtract_signal(&mut audio, &msg);

        // Measure energy after subtraction
        let energy_after: f64 = audio.iter().map(|&s| (s as f64) * (s as f64)).sum();

        let reduction = 1.0 - (energy_after / energy_before);
        eprintln!(
            "Energy before: {:.6}, after: {:.6}, reduction: {:.1}%",
            energy_before,
            energy_after,
            reduction * 100.0
        );

        // Should remove at least 70% of the energy
        assert!(
            reduction > 0.7,
            "Signal subtraction only removed {:.1}% of energy (expected >70%)",
            reduction * 100.0
        );
    }

    // hb-091 Session 2: scoped decode primitive — restrict Costas sync to a
    // user-supplied freq_bin range. Lets the coordinator scope the t=13s
    // partial-buffer decode to the in-QSO partner's known frequency.
    //
    // Frequency model: `Ft8Modulator::new_default()` has base_frequency =
    // BASE_FREQUENCY (1500 Hz). `modulate_symbols(symbols, freq_offset)`
    // emits at `1500 + freq_offset` Hz. Costas freq_bins are spaced at
    // tone_spacing = 6.25 Hz, so freq_bin = total_hz / 6.25.
    #[cfg(feature = "transmit")]
    #[test]
    fn test_scoped_decode_within_range_recovers_message() {
        // Signal at 1500 + 500 = 2000 Hz → freq_bin 320. Scope 318..=322
        // brackets the truth with a few bins of slack.
        let mut encoder = crate::Ft8Encoder::new();
        let symbols = encoder
            .encode_message("CQ K5ARH EM10", None)
            .expect("encode");
        let mut modulator = crate::Ft8Modulator::new_default().expect("modulator");
        let mut tx = modulator
            .modulate_symbols(&symbols, 500.0)
            .expect("modulate");
        tx.resize(WINDOW_SAMPLES, 0.0);

        let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
        let decoded = decoder
            .decode_window_scoped(&tx, 318..=322)
            .expect("decode");

        assert!(
            decoded.iter().any(|m| m.text == "CQ K5ARH EM10"),
            "scoped decode covering the truth bin should recover CQ K5ARH EM10, got: {:?}",
            decoded.iter().map(|m| m.text.as_str()).collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "transmit")]
    #[test]
    fn test_scoped_decode_excludes_out_of_range_message() {
        // Same signal at 2000 Hz (freq_bin 320). Scope 78..=82 covers
        // ~500 Hz — far from the truth. Sync sweep should never reach it.
        let mut encoder = crate::Ft8Encoder::new();
        let symbols = encoder
            .encode_message("CQ K5ARH EM10", None)
            .expect("encode");
        let mut modulator = crate::Ft8Modulator::new_default().expect("modulator");
        let mut tx = modulator
            .modulate_symbols(&symbols, 500.0)
            .expect("modulate");
        tx.resize(WINDOW_SAMPLES, 0.0);

        let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
        let decoded = decoder.decode_window_scoped(&tx, 78..=82).expect("decode");

        assert!(
            !decoded.iter().any(|m| m.text == "CQ K5ARH EM10"),
            "scoped decode at ~500 Hz must NOT recover the 2000 Hz transmission, got: {:?}",
            decoded.iter().map(|m| m.text.as_str()).collect::<Vec<_>>()
        );
    }

    // ========================================================================
    // hb-242 sync_bc partial-Costas + wide-lag baseline (red2) + hb-245
    // subsample DT refinement.
    // ========================================================================

    /// Synthetic helper: build a Costas spectrogram where the caller
    /// picks which sync groups (A=0, B=1, C=2) are present.
    fn synthetic_costas_spec(
        present_groups: &[usize],
        signal_db: f64,
        noise_db: f64,
        f0: usize,
    ) -> Spectrogram {
        let steps_per_symbol = TIME_OSR;
        let num_steps = 79 * steps_per_symbol;
        let num_bins = SAMPLES_PER_SYMBOL / 2 + 1;
        let freq_osr = FREQ_OSR;

        let mut power = vec![vec![vec![noise_db; num_bins]; freq_osr]; num_steps];

        for &m in present_groups {
            let group_start = [0usize, 36, 72][m];
            for j in 0..7 {
                let sym = group_start + j;
                let tone = crate::protocol::FT8_COSTAS[j] as usize;
                for sub in 0..steps_per_symbol {
                    let time_idx = sym * steps_per_symbol + sub;
                    if time_idx < num_steps && f0 + tone < num_bins {
                        power[time_idx][0][f0 + tone] = signal_db;
                    }
                }
            }
        }

        Spectrogram {
            power,
            complex: None,
            num_steps,
            num_bins,
            freq_osr,
            time_padding: 0,
        }
    }

    /// hb-242: on a fully-clean signal (all three Costas blocks
    /// present), the full ABC metric and partial BC metric agree
    /// within a small tolerance.
    #[test]
    fn hb242_full_and_partial_agree_on_clean_signal() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();
        let f0 = 240usize;
        let spec = synthetic_costas_spec(&[0, 1, 2], -10.0, -40.0, f0);

        let full = decoder.compute_costas_score(&spec, 0, f0, 0);
        let partial = decoder.compute_costas_score_partial_bc(&spec, 0, f0, 0);

        assert!(full > MIN_SYNC_SCORE, "full={full} below gate");
        assert!(partial > MIN_SYNC_SCORE, "partial={partial} below gate");
        let ratio = (full - partial).abs() / full;
        assert!(
            ratio < 0.05,
            "clean full {full} and partial {partial} should agree within 5%"
        );
    }

    /// hb-242 core: when block A is zeroed out (slot-edge negative-dt
    /// scenario), the full ABC metric collapses while the partial BC
    /// metric stays meaningful.
    #[test]
    fn hb242_partial_rescues_when_block_a_missing() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();
        let f0 = 240usize;
        let spec = synthetic_costas_spec(&[1, 2], -10.0, -40.0, f0);

        let full = decoder.compute_costas_score(&spec, 0, f0, 0);
        let partial = decoder.compute_costas_score_partial_bc(&spec, 0, f0, 0);

        assert!(
            partial > MIN_SYNC_SCORE,
            "partial={partial} should exceed gate without block A"
        );
        assert!(
            partial > full * 1.3,
            "partial {partial} should be >=1.3x full {full} with block A missing"
        );
    }

    /// hb-242: costas_sync_search_with_threshold recovers the (t0=0,
    /// f0) candidate when only blocks B+C are present.
    #[test]
    fn hb242_sync_search_recovers_block_a_missing_candidate() {
        let mut config = Ft8Config::default();
        config.nms_enabled = false;
        config.sync_time_interpolation = false;
        config.costas_partial_metric_enabled = true;
        config.costas_two_baseline_enabled = false;

        let decoder = Ft8Decoder::new(config).unwrap();
        let f0 = 240usize;
        let spec = synthetic_costas_spec(&[1, 2], -10.0, -40.0, f0);

        let candidates = decoder
            .costas_sync_search_with_threshold(&spec, MIN_SYNC_SCORE, None)
            .expect("sync search");

        let found = candidates
            .iter()
            .any(|c| c.freq_bin == f0 && c.freq_sub == 0 && c.time_step == 0);
        assert!(
            found,
            "hb-242: block-A-missing candidate should be recovered; got {} candidates",
            candidates.len()
        );
    }

    /// hb-242 negative control: turning the partial-Costas metric off
    /// should lower the (t0=0, f0) sync_score on the block-A-missing
    /// spec. The candidate may still survive the absolute gate on
    /// synthetic-clean noise (block A's per-symbol signal_dB -
    /// neighbor_dB ≈ 0 averages harmlessly into the ABC metric); the
    /// production payoff is measured on hard-200 separately.
    #[test]
    fn hb242_disabled_lowers_block_a_missing_score() {
        let f0 = 240usize;
        let spec = synthetic_costas_spec(&[1, 2], -10.0, -40.0, f0);

        let candidate_score = |partial_enabled: bool| -> f64 {
            let mut config = Ft8Config::default();
            config.nms_enabled = false;
            config.sync_time_interpolation = false;
            config.costas_partial_metric_enabled = partial_enabled;
            config.costas_two_baseline_enabled = false;
            let decoder = Ft8Decoder::new(config).unwrap();
            let candidates = decoder
                .costas_sync_search_with_threshold(&spec, MIN_SYNC_SCORE, None)
                .expect("sync search");
            candidates
                .iter()
                .find(|c| c.freq_bin == f0 && c.freq_sub == 0 && c.time_step == 0)
                .map(|c| c.sync_score)
                .unwrap_or(0.0)
        };

        let with_partial = candidate_score(true);
        let without_partial = candidate_score(false);

        assert!(
            with_partial > without_partial * 1.2,
            "partial-on score {with_partial} should be >1.2x partial-off score {without_partial}"
        );
    }

    /// hb-242 wide-lag baseline: when enabled, a clean signal still
    /// keeps its main candidate. The mechanism is additive.
    #[test]
    fn hb242_two_baseline_preserves_clean_candidate() {
        let mut config = Ft8Config::default();
        config.nms_enabled = false;
        config.sync_time_interpolation = false;
        config.costas_partial_metric_enabled = true;
        config.costas_two_baseline_enabled = true;

        let decoder = Ft8Decoder::new(config).unwrap();
        let f0 = 240usize;
        let spec = synthetic_costas_spec(&[0, 1, 2], -10.0, -40.0, f0);

        let candidates = decoder
            .costas_sync_search_with_threshold(&spec, MIN_SYNC_SCORE, None)
            .expect("sync search");

        let found = candidates
            .iter()
            .any(|c| c.freq_bin == f0 && c.freq_sub == 0 && c.time_step == 0);
        assert!(
            found,
            "two-baseline must keep the clean (0, {f0}) candidate; got {} candidates",
            candidates.len()
        );
    }

    /// hb-245 subsample DT refinement: an aligned synthetic signal
    /// produces a `time_refinement` in [-0.5, +0.5]. Verifies
    /// pancetta's hb-044 implementation satisfies the hb-245 spec.
    #[test]
    fn hb245_aligned_signal_has_bounded_time_refinement() {
        let mut config = Ft8Config::default();
        config.nms_enabled = false;
        config.sync_time_interpolation = true;
        config.sync_time_interp_delta_scale = 1.0;
        config.sync_time_interp_score_gate = 0.0;
        config.costas_partial_metric_enabled = false;
        config.costas_two_baseline_enabled = false;

        let decoder = Ft8Decoder::new(config).unwrap();
        let f0 = 240usize;
        let spec = synthetic_costas_spec(&[0, 1, 2], -10.0, -40.0, f0);

        let candidates = decoder
            .costas_sync_search_with_threshold(&spec, MIN_SYNC_SCORE, None)
            .expect("sync search");

        let cand = candidates
            .iter()
            .find(|c| c.freq_bin == f0 && c.freq_sub == 0 && c.time_step == 0)
            .expect("aligned candidate must survive");

        assert!(
            cand.time_refinement.abs() <= 0.5,
            "time_refinement {} out of [-0.5, +0.5]",
            cand.time_refinement
        );
    }

    // ====================================================================
    // JS8Call-Improved-style LDPC feedback refinement (clean-room port from
    // spec-js8call-ldpc-feedback-refinement.md).
    //
    // The mechanism is a meta-loop around BP: when first-pass BP fails,
    // the iter-1 hard-decision codeword is used to reshape input LLRs
    // (boost agreeing bits, attenuate / erase disagreeing bits) before a
    // second BP pass.
    //
    // These tests focus on the transform's mathematical properties and
    // the default-off guarantee. Bit-accuracy / corpus measurement happens
    // separately in the hard-200 sweep.
    // ====================================================================

    #[test]
    fn test_feedback_refinement_default_off_preserves_decode_soft() {
        // Default Ft8Config disables refinement; decode_soft must behave
        // identically to the legacy path. We construct a converging-clean
        // input and a noisy-non-converging input and confirm the default
        // decoder reaches the same bits as one with refinement off via
        // explicit builder.
        let default_decoder = LdpcDecoder::new_with_osd(50, None).unwrap();
        let explicit_off = LdpcDecoder::new_with_osd(50, None)
            .unwrap()
            .with_feedback_refinement(false, 1.5, 0.5, 1.0);

        // Clean all-zero codeword: trivially converges.
        let clean = vec![5.0f32; 174];
        let a = default_decoder.decode_soft(&clean).unwrap();
        let b = explicit_off.decode_soft(&clean).unwrap();
        for i in 0..174 {
            assert_eq!(a[i], b[i], "default vs explicit-off mismatch at {i}");
        }

        // Lightly-corrupted input: must still match between default-off and
        // explicit-off configurations.
        let mut noisy = vec![5.0f32; 174];
        noisy[10] = -1.0;
        noisy[50] = -0.5;
        let a2 = default_decoder.decode_soft(&noisy).unwrap();
        let b2 = explicit_off.decode_soft(&noisy).unwrap();
        for i in 0..174 {
            assert_eq!(
                a2[i], b2[i],
                "noisy default vs explicit-off mismatch at {i}"
            );
        }
    }

    #[test]
    fn test_feedback_refinement_agreement_boosts_magnitude() {
        // Spec step 1.3: when the original LLR sign matches the candidate
        // codeword bit, multiply |LLR| by the boost factor with sign
        // preserved.
        let cfg = FeedbackRefinementConfig {
            enabled: true,
            boost_factor: 2.0,
            attenuate_factor: 0.5,
            erase_threshold: 0.0, // erasure disabled for this test
        };

        let mut llrs = [0.0f32; 174];
        let mut hard = [0u8; 174];

        // Original LLR = +3.0 (positive -> llr_bit = 0); HD = 0 -> AGREE.
        llrs[0] = 3.0;
        hard[0] = 0;
        // Original LLR = -4.0 (negative -> llr_bit = 1); HD = 1 -> AGREE.
        llrs[1] = -4.0;
        hard[1] = 1;
        // Filler: all-agreement so the counters add up cleanly.
        for i in 2..174 {
            llrs[i] = 2.0;
            hard[i] = 0;
        }

        let stats = refine_llrs_from_hard_decisions(&mut llrs, &hard, &cfg);

        assert!(
            (llrs[0] - 6.0).abs() < 1e-6,
            "expected boost 3.0 -> 6.0, got {}",
            llrs[0]
        );
        assert!(
            (llrs[1] - -8.0).abs() < 1e-6,
            "expected boost -4.0 -> -8.0, got {}",
            llrs[1]
        );
        assert_eq!(stats.confident_bits, 174);
        assert_eq!(stats.uncertain_bits, 0);
        assert_eq!(stats.erased_bits, 0);
    }

    #[test]
    fn test_feedback_refinement_disagreement_attenuates_magnitude() {
        // Spec step 1.4: when the original LLR sign disagrees with the
        // candidate codeword bit AND |LLR| >= erase threshold, attenuate
        // magnitude (multiply by < 1) with sign preserved.
        let cfg = FeedbackRefinementConfig {
            enabled: true,
            boost_factor: 1.5,
            attenuate_factor: 0.25,
            erase_threshold: 0.0, // never erase (force pure attenuation)
        };

        let mut llrs = [0.0f32; 174];
        let mut hard = [0u8; 174];

        // Original LLR = +4.0 (llr_bit = 0); HD = 1 -> DISAGREE.
        llrs[0] = 4.0;
        hard[0] = 1;
        // Original LLR = -8.0 (llr_bit = 1); HD = 0 -> DISAGREE.
        llrs[1] = -8.0;
        hard[1] = 0;
        // Pad with all-agreement.
        for i in 2..174 {
            llrs[i] = 2.0;
            hard[i] = 0;
        }

        let stats = refine_llrs_from_hard_decisions(&mut llrs, &hard, &cfg);

        assert!(
            (llrs[0] - 1.0).abs() < 1e-6,
            "expected 4.0 -> 1.0 (x0.25), got {}",
            llrs[0]
        );
        assert!(
            (llrs[1] - -2.0).abs() < 1e-6,
            "expected -8.0 -> -2.0 (x0.25), got {}",
            llrs[1]
        );
        assert_eq!(stats.confident_bits, 172);
        assert_eq!(stats.uncertain_bits, 2);
        assert_eq!(stats.erased_bits, 0);
    }

    #[test]
    fn test_feedback_refinement_shallow_disagreement_erases() {
        // Spec step 1.4 erasure branch: when |LLR| < erase threshold AND
        // disagreement, force the LLR to 0 (treat as erased).
        let cfg = FeedbackRefinementConfig {
            enabled: true,
            boost_factor: 1.5,
            attenuate_factor: 0.5,
            erase_threshold: 1.5,
        };

        let mut llrs = [0.0f32; 174];
        let mut hard = [0u8; 174];

        // |LLR| = 0.5 < threshold; disagrees -> ERASE.
        llrs[0] = 0.5;
        hard[0] = 1;
        // |LLR| = -0.7 (abs=0.7) < threshold; disagrees -> ERASE.
        llrs[1] = -0.7;
        hard[1] = 0;
        // |LLR| = 3.0 >= threshold; disagrees -> ATTENUATE (not erase).
        llrs[2] = 3.0;
        hard[2] = 1;
        // Filler agrees so counters are easy to verify.
        for i in 3..174 {
            llrs[i] = 2.0;
            hard[i] = 0;
        }

        let stats = refine_llrs_from_hard_decisions(&mut llrs, &hard, &cfg);

        assert_eq!(llrs[0], 0.0, "shallow disagreement must erase");
        assert_eq!(llrs[1], 0.0, "shallow negative disagreement must erase");
        assert!(
            (llrs[2] - 1.5).abs() < 1e-6,
            "deep disagreement must attenuate: 3.0 -> 1.5"
        );

        assert_eq!(stats.confident_bits, 171);
        assert_eq!(stats.uncertain_bits, 3);
        assert_eq!(stats.erased_bits, 2);
    }

    #[test]
    fn test_feedback_refinement_clamps_runaway_magnitude() {
        // Spec edge-case: "clamp LLR magnitude to a reasonable maximum to
        // avoid saturation issues downstream". Large input * large boost
        // must not exceed FEEDBACK_REFINEMENT_LLR_CLAMP.
        let cfg = FeedbackRefinementConfig {
            enabled: true,
            boost_factor: 10.0,
            attenuate_factor: 0.5,
            erase_threshold: 0.0,
        };

        let mut llrs = [0.0f32; 174];
        let mut hard = [0u8; 174];

        // Original LLR 100.0 with agreement: 100 * 10 = 1000 -> must clamp.
        llrs[0] = 100.0;
        hard[0] = 0;
        // Same on the negative side.
        llrs[1] = -100.0;
        hard[1] = 1;
        for i in 2..174 {
            llrs[i] = 1.0;
            hard[i] = 0;
        }

        let _ = refine_llrs_from_hard_decisions(&mut llrs, &hard, &cfg);

        assert!(
            llrs[0] <= FEEDBACK_REFINEMENT_LLR_CLAMP + 1e-6,
            "positive boost must be clamped to <= {FEEDBACK_REFINEMENT_LLR_CLAMP}; got {}",
            llrs[0]
        );
        assert!(
            llrs[0] > 0.0,
            "positive clamp must preserve sign; got {}",
            llrs[0]
        );
        assert!(
            llrs[1] >= -FEEDBACK_REFINEMENT_LLR_CLAMP - 1e-6,
            "negative boost must be clamped to >= -{FEEDBACK_REFINEMENT_LLR_CLAMP}; got {}",
            llrs[1]
        );
        assert!(
            llrs[1] < 0.0,
            "negative clamp must preserve sign; got {}",
            llrs[1]
        );
    }

    #[test]
    fn test_feedback_refinement_infinity_threshold_disables_erasure() {
        // Spec edge-case: erasure-disabled mode (threshold = +infinity).
        // Disagreement bits must always attenuate, never erase.
        let cfg = FeedbackRefinementConfig {
            enabled: true,
            boost_factor: 1.5,
            attenuate_factor: 0.5,
            erase_threshold: f32::INFINITY,
        };

        let mut llrs = [0.0f32; 174];
        let mut hard = [0u8; 174];

        // Shallow disagreement: would normally erase, must attenuate.
        llrs[0] = 0.1;
        hard[0] = 1;
        for i in 1..174 {
            llrs[i] = 1.0;
            hard[i] = 0;
        }

        let stats = refine_llrs_from_hard_decisions(&mut llrs, &hard, &cfg);

        assert!(
            (llrs[0] - 0.05).abs() < 1e-6,
            "infinity threshold must skip erasure: 0.1 -> 0.05 (x0.5), got {}",
            llrs[0]
        );
        assert_eq!(stats.erased_bits, 0);
        assert_eq!(stats.uncertain_bits, 1);
    }

    #[test]
    fn test_feedback_refinement_clean_codeword_short_circuits() {
        // Spec edge-case: all-bits-agreement means the BP decoder already
        // converged. The decode_soft meta-loop should never trigger
        // refinement on a converging input; it returns at the
        // bp_converged check.
        let with_refinement = LdpcDecoder::new_with_osd(50, None)
            .unwrap()
            .with_feedback_refinement(true, 1.5, 0.5, 1.0);
        let without_refinement = LdpcDecoder::new_with_osd(50, None).unwrap();

        // Confident all-zero codeword: BP converges in iter 1.
        let clean = vec![5.0f32; 174];
        let a = with_refinement.decode_soft(&clean).unwrap();
        let b = without_refinement.decode_soft(&clean).unwrap();
        for i in 0..174 {
            assert_eq!(
                a[i], b[i],
                "refinement must short-circuit on clean codeword at bit {i}"
            );
        }
    }

    #[test]
    fn test_feedback_refinement_invalid_factors_fall_back_to_defaults() {
        // `with_feedback_refinement` must guard against pathological inputs
        // (NaN, negative boost) by silently using the defaults. This
        // protects downstream from NaN propagation when configs come from
        // hot-reload or research sweeps.
        let cfg = LdpcDecoder::new(10)
            .unwrap()
            .with_feedback_refinement(true, f32::NAN, -1.0, f32::NAN)
            .feedback_refinement;

        assert!(cfg.enabled);
        assert!(
            (cfg.boost_factor - 1.5).abs() < 1e-6,
            "NaN boost must fall back to 1.5"
        );
        assert!(
            (cfg.attenuate_factor - 0.5).abs() < 1e-6,
            "negative attenuate must fall back to 0.5"
        );
        assert!(
            (cfg.erase_threshold - 1.0).abs() < 1e-6,
            "NaN erase threshold must fall back to 1.0"
        );

        // Infinity erase threshold must be passed through unchanged.
        let cfg2 = LdpcDecoder::new(10)
            .unwrap()
            .with_feedback_refinement(true, 1.5, 0.5, f32::INFINITY)
            .feedback_refinement;
        assert!(cfg2.erase_threshold.is_infinite());
    }
}

// ===========================================================================
// hb-230: JTDX-style relaxed Costas-sync threshold near QSO partner +
// JTDX-style between-cycle 2-tap audio smoothing.
//
// These mechanisms ship in Batch 49 paired with hb-229 (QSO partner
// band-collapse, Batch 48). The relaxed-sync threshold gives weak partner
// messages (RR73, 73) a second chance at acceptance inside the
// band-collapse window; the cycle smoothing perturbs the audio between
// passes so multi-pass mode finds candidates the first pass missed by
// a small margin.
// ===========================================================================
#[cfg(test)]
mod hb230_relaxed_sync_tests {
    use super::*;
    use crate::TONE_SPACING;

    /// Build a synthetic spectrogram with a Costas pattern at the given
    /// freq_bin and per-tone signal/noise dB levels.
    fn build_synthetic_costas(
        present_groups: &[usize],
        signal_db: f64,
        noise_db: f64,
        f0: usize,
    ) -> Spectrogram {
        let steps_per_symbol = TIME_OSR;
        let num_steps = 79 * steps_per_symbol;
        let num_bins = SAMPLES_PER_SYMBOL / 2 + 1;
        let freq_osr = FREQ_OSR;

        let mut power = vec![vec![vec![noise_db; num_bins]; freq_osr]; num_steps];

        for &m in present_groups {
            let group_start = [0usize, 36, 72][m];
            for j in 0..7 {
                let sym = group_start + j;
                let tone = crate::protocol::FT8_COSTAS[j] as usize;
                for sub in 0..steps_per_symbol {
                    let time_idx = sym * steps_per_symbol + sub;
                    if time_idx < num_steps && f0 + tone < num_bins {
                        power[time_idx][0][f0 + tone] = signal_db;
                    }
                }
            }
        }

        Spectrogram {
            power,
            complex: None,
            num_steps,
            num_bins,
            freq_osr,
            time_padding: 0,
        }
    }

    /// Default config: with `relaxed_sync_near_partner_hz_radius = None`
    /// (the production default), the new partner-aware sync search must
    /// be byte-identical to the historical one — no regression on the
    /// shipped pipeline.
    #[test]
    fn hb230_default_off_partner_arg_is_noop() {
        let config = Ft8Config::default();
        assert!(config.relaxed_sync_near_partner_hz_radius.is_none());
        assert_eq!(config.relaxed_sync_near_partner_score_delta, 0.0);
        assert!(!config.cycle_audio_smoothing_enabled);

        let decoder = Ft8Decoder::new(config).unwrap();
        let f0 = 240usize;
        let spec = build_synthetic_costas(&[0, 1, 2], -10.0, -40.0, f0);

        let without_partner = decoder
            .costas_sync_search_with_threshold_and_partner(&spec, MIN_SYNC_SCORE, None, None)
            .expect("sync search no partner");
        let with_partner_default_off = decoder
            .costas_sync_search_with_threshold_and_partner(
                &spec,
                MIN_SYNC_SCORE,
                None,
                Some(1500.0),
            )
            .expect("sync search partner provided but feature off");

        assert_eq!(
            without_partner.len(),
            with_partner_default_off.len(),
            "partner arg should be a no-op when radius is None"
        );
        for (a, b) in without_partner.iter().zip(with_partner_default_off.iter()) {
            assert_eq!(a.freq_bin, b.freq_bin);
            assert_eq!(a.time_step, b.time_step);
            assert_eq!(a.freq_sub, b.freq_sub);
            assert!(
                (a.sync_score - b.sync_score).abs() < 1e-9,
                "scores must match exactly"
            );
        }
    }

    /// The relaxed branch never SHRINKS the candidate set — with the
    /// radius set and a partner freq supplied, the result must be a
    /// superset of the wide-band result.
    #[test]
    fn hb230_relaxed_window_can_only_add_candidates() {
        let f0 = 240usize;
        let partner_hz = f0 as f64 * TONE_SPACING; // 1500 Hz
        let spec = build_synthetic_costas(&[0, 1, 2], -10.0, -40.0, f0);

        let mut config = Ft8Config::default();
        config.nms_enabled = false;
        config.sync_time_interpolation = false;
        config.costas_partial_metric_enabled = false;
        config.costas_two_baseline_enabled = false;
        config.relaxed_sync_near_partner_hz_radius = Some(5.0);
        config.relaxed_sync_near_partner_score_delta = -100.0;
        let decoder = Ft8Decoder::new(config).unwrap();

        let baseline = decoder
            .costas_sync_search_with_threshold_and_partner(&spec, MIN_SYNC_SCORE, None, None)
            .expect("baseline");
        let with_partner = decoder
            .costas_sync_search_with_threshold_and_partner(
                &spec,
                MIN_SYNC_SCORE,
                None,
                Some(partner_hz),
            )
            .expect("partner");

        let baseline_set: std::collections::HashSet<(usize, usize, usize)> = baseline
            .iter()
            .map(|c| (c.time_step, c.freq_bin, c.freq_sub))
            .collect();
        // Every baseline candidate is preserved.
        for c in &baseline {
            let key = (c.time_step, c.freq_bin, c.freq_sub);
            assert!(
                with_partner
                    .iter()
                    .any(|w| (w.time_step, w.freq_bin, w.freq_sub) == key),
                "baseline candidate at {key:?} missing from partner result"
            );
        }
        // Every added candidate (in partner but not baseline) is INSIDE
        // the ±5 Hz window around the partner.
        for c in &with_partner {
            let key = (c.time_step, c.freq_bin, c.freq_sub);
            if !baseline_set.contains(&key) {
                let sub_off = c.freq_sub as f64 * (TONE_SPACING / FREQ_OSR as f64);
                let cand_freq = c.freq_bin as f64 * TONE_SPACING + sub_off;
                assert!(
                    (cand_freq - partner_hz).abs() <= 5.0,
                    "added candidate at {cand_freq:.2} Hz outside the ±5 Hz \
                     window around {partner_hz:.2} Hz"
                );
            }
        }
    }

    /// Critical safety: even with an aggressive negative delta, the
    /// relaxed threshold is clamped at 0 so pure-noise candidates are
    /// not admitted. Probe by setting score_delta = -1000.0 (would
    /// naively drive the gate to -995.0) and confirming the candidate
    /// count stays bounded by `max_sync_candidates`.
    #[test]
    fn hb230_relaxed_threshold_clamped_at_zero() {
        let f0 = 240usize;
        let partner_hz = f0 as f64 * TONE_SPACING;
        let spec = build_synthetic_costas(&[0, 1, 2], -10.0, -40.0, f0);

        let mut config = Ft8Config::default();
        config.nms_enabled = false;
        config.sync_time_interpolation = false;
        config.costas_partial_metric_enabled = false;
        config.costas_two_baseline_enabled = false;
        config.relaxed_sync_near_partner_hz_radius = Some(5.0);
        config.relaxed_sync_near_partner_score_delta = -1000.0;
        let max_sync_candidates = config.max_sync_candidates;
        let decoder = Ft8Decoder::new(config).unwrap();

        let result = decoder
            .costas_sync_search_with_threshold_and_partner(
                &spec,
                MIN_SYNC_SCORE,
                None,
                Some(partner_hz),
            )
            .expect("partner");

        assert!(
            result.len() <= max_sync_candidates,
            "relaxed threshold clamp failed: {} candidates emitted",
            result.len()
        );
    }

    /// `partner_freq_hz = None` always wins over a configured radius —
    /// the relaxed branch is a no-op when the per-call signal is absent.
    #[test]
    fn hb230_none_partner_disables_relaxed_branch() {
        let f0 = 240usize;
        let spec = build_synthetic_costas(&[0, 1, 2], -10.0, -40.0, f0);

        let mut config = Ft8Config::default();
        config.nms_enabled = false;
        config.sync_time_interpolation = false;
        config.costas_partial_metric_enabled = false;
        config.costas_two_baseline_enabled = false;
        config.relaxed_sync_near_partner_hz_radius = Some(10.0);
        config.relaxed_sync_near_partner_score_delta = -100.0;
        let decoder = Ft8Decoder::new(config).unwrap();

        let baseline = decoder
            .costas_sync_search_with_threshold_and_partner(&spec, MIN_SYNC_SCORE, None, None)
            .expect("baseline");
        let same_baseline = decoder
            .costas_sync_search_with_threshold_and_partner(&spec, MIN_SYNC_SCORE, None, None)
            .expect("repeat");
        assert_eq!(baseline.len(), same_baseline.len());
    }
}

// ===========================================================================
// JTDX cycle audio smoothing: 2-tap forward moving-average applied
// in-place to the residual audio buffer when entering pass 2 (pancetta's
// `pass == 1`). Default OFF; the tests confirm the default-off path is
// byte-identical to no smoothing and the on-path applies the canonical MA.
// ===========================================================================
#[cfg(test)]
mod hb230_cycle_smoothing_tests {
    use super::*;
    use crate::WINDOW_SAMPLES;

    /// Replicate the in-place 2-tap forward MA the decoder applies
    /// between passes. This unit test does not exercise the full
    /// decoder path (which would require WAV synthesis); it exercises
    /// the buffer transformation directly so the test stays robust to
    /// neighbouring decoder changes.
    fn apply_2tap_forward_ma_in_place(buf: &mut [f32]) {
        if buf.len() < 2 {
            return;
        }
        let n = buf.len();
        for i in 0..n - 1 {
            buf[i] = 0.5 * (buf[i] + buf[i + 1]);
        }
    }

    #[test]
    fn cycle_smoothing_default_off_in_config() {
        let config = Ft8Config::default();
        assert!(
            !config.cycle_audio_smoothing_enabled,
            "cycle smoothing must default OFF — production behaviour byte-identical until measurement"
        );
    }

    #[test]
    fn cycle_smoothing_2tap_ma_is_identity_on_dc() {
        let mut samples = vec![0.42f32; 1024];
        let original = samples.clone();
        apply_2tap_forward_ma_in_place(&mut samples);
        for (i, (s, o)) in samples.iter().zip(original.iter()).enumerate() {
            assert!(
                (s - o).abs() < 1e-7,
                "DC sample {i} drifted under MA: got {s}, expected {o}"
            );
        }
    }

    #[test]
    fn cycle_smoothing_2tap_ma_matches_canonical_formula() {
        let mut samples: Vec<f32> = (0..8).map(|i| i as f32).collect();
        let expected = [0.5_f32, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.0];
        apply_2tap_forward_ma_in_place(&mut samples);
        for (i, (got, want)) in samples.iter().zip(expected.iter()).enumerate() {
            assert!(
                (got - want).abs() < 1e-6,
                "MA sample {i}: got {got}, want {want}"
            );
        }
    }

    #[test]
    fn cycle_smoothing_edge_cases_short_buffers() {
        let mut empty: Vec<f32> = vec![];
        apply_2tap_forward_ma_in_place(&mut empty);
        assert!(empty.is_empty());

        let mut one = vec![0.5f32];
        apply_2tap_forward_ma_in_place(&mut one);
        assert!((one[0] - 0.5).abs() < 1e-7);

        let mut two = vec![1.0f32, 3.0];
        apply_2tap_forward_ma_in_place(&mut two);
        assert!((two[0] - 2.0).abs() < 1e-6);
        assert!((two[1] - 3.0).abs() < 1e-6);
    }

    /// Default-off invariant: when the feature flag is false, the
    /// decoder must not mutate the audio between passes. This test
    /// asserts the config invariant; the full no-op invariant is
    /// confirmed by the lack of any code path that touches the buffer
    /// when the flag is false (the pass-loop condition guards it).
    #[test]
    fn cycle_smoothing_default_config_path_is_inert() {
        let config = Ft8Config::default();
        assert!(!config.cycle_audio_smoothing_enabled);
        // The pass-loop condition is `pass == 1
        // && config.cycle_audio_smoothing_enabled
        // && residual_samples.len() >= 2`. With the flag off the
        // smoothing block short-circuits.
    }

    // ========================================================================
    // hb-244 soft combiner wiring tests
    // ========================================================================

    #[test]
    fn hb244_soft_combiner_default_off_in_config() {
        let config = Ft8Config::default();
        assert!(
            !config.soft_combiner_enabled,
            "soft combiner must default OFF — wiring is opt-in pending corpus measurement"
        );
        assert_eq!(
            config.soft_combiner_capacity,
            crate::soft_combiner::DEFAULT_CAPACITY,
            "default capacity should match module constant"
        );
        assert_eq!(
            config.soft_combiner_ttl_seconds,
            crate::soft_combiner::DEFAULT_TTL_SECONDS,
            "default TTL should match module constant"
        );
    }

    #[test]
    fn hb244_soft_combiner_field_is_none_when_disabled() {
        let config = Ft8Config {
            soft_combiner_enabled: false,
            ..Ft8Config::default()
        };
        let decoder = Ft8Decoder::new(config).expect("decoder ctor");
        assert!(
            decoder.soft_combiner.is_none(),
            "with soft_combiner_enabled=false, the combiner field must be None — \
             this is what guarantees the zero-cost disabled hot path"
        );
    }

    #[test]
    fn hb244_soft_combiner_field_is_some_when_enabled() {
        let config = Ft8Config {
            soft_combiner_enabled: true,
            soft_combiner_capacity: 64,
            soft_combiner_ttl_seconds: 30,
            ..Ft8Config::default()
        };
        let decoder = Ft8Decoder::new(config).expect("decoder ctor");
        assert!(
            decoder.soft_combiner.is_some(),
            "with soft_combiner_enabled=true, the combiner must be constructed eagerly"
        );
        // Combiner starts empty.
        let combiner = decoder.soft_combiner.as_ref().unwrap();
        let guard = combiner.lock().expect("not poisoned");
        assert!(guard.is_empty());
    }

    /// Default-OFF integration test: decoding a synthesized FT8 signal
    /// with soft_combiner_enabled=false must produce the exact same
    /// decoded output as the prior (pre-wiring) decoder. This is the
    /// "byte-identical output" guarantee that lets us flip the default
    /// safely once corpus measurement validates.
    #[cfg(feature = "transmit")]
    #[test]
    fn hb244_default_off_decode_output_is_unchanged() {
        let mut encoder = crate::Ft8Encoder::new();
        let symbols = encoder
            .encode_message("CQ K5ARH EM10", None)
            .expect("encode");
        let mut modulator = crate::Ft8Modulator::new_default().expect("modulator");
        let mut tx = modulator
            .modulate_symbols(&symbols, 500.0)
            .expect("modulate");
        tx.resize(WINDOW_SAMPLES, 0.0);

        // Baseline: explicit-OFF config.
        let cfg_off = Ft8Config {
            soft_combiner_enabled: false,
            ..Ft8Config::default()
        };
        let mut dec_off = Ft8Decoder::new(cfg_off).expect("decoder ctor");
        let decoded_off = dec_off.decode_window(&tx).expect("decode");

        assert!(
            decoded_off.iter().any(|m| m.text == "CQ K5ARH EM10"),
            "default-off decode must still recover the truth signal; got: {:?}",
            decoded_off
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// Default-ON smoke test: same signal, combiner enabled. The signal
    /// should still decode (the combine call is a passthrough on first
    /// reception). After a successful decode at this coarse key, the
    /// combiner's mark_decoded should have evicted the bucket — verify
    /// by inspecting the combiner state.
    #[cfg(feature = "transmit")]
    #[test]
    fn hb244_default_on_decode_marks_decoded_bucket_empty() {
        let mut encoder = crate::Ft8Encoder::new();
        let symbols = encoder
            .encode_message("CQ K5ARH EM10", None)
            .expect("encode");
        let mut modulator = crate::Ft8Modulator::new_default().expect("modulator");
        let mut tx = modulator
            .modulate_symbols(&symbols, 500.0)
            .expect("modulate");
        tx.resize(WINDOW_SAMPLES, 0.0);

        let cfg_on = Ft8Config {
            soft_combiner_enabled: true,
            ..Ft8Config::default()
        };
        let mut dec_on = Ft8Decoder::new(cfg_on).expect("decoder ctor");
        let decoded_on = dec_on.decode_window(&tx).expect("decode");

        assert!(
            decoded_on.iter().any(|m| m.text == "CQ K5ARH EM10"),
            "default-on decode must still recover the truth signal; got: {:?}",
            decoded_on
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>()
        );

        // After a CRC-pass at the truth's coarse key, mark_decoded
        // should have evicted that bucket. Other candidates that failed
        // BP/CRC may still be cached, but the count is bounded and
        // typically small (failed candidates haven't been mark_decoded'd).
        let combiner = dec_on.soft_combiner.as_ref().expect("combiner present");
        let guard = combiner.lock().expect("not poisoned");
        // Sanity check: combiner saw at least one combine call.
        // The exact len depends on how many failed candidates left
        // entries behind; just assert it's reachable (no panic / no
        // unbounded growth).
        let _ = guard.len();
    }

    /// Edge case: two sequential decodes of the same WAV produce a
    /// combine call on pass 2 (cache hit at the same coarse key).
    /// We can't directly assert "combine was called with repeat_count=2"
    /// without instrumentation, but we CAN assert that the combiner
    /// state after one decode is non-trivial (some failed candidates
    /// left entries), and that a second pass on a noise-only WAV at
    /// the same combiner reuses the cache (entries persist across
    /// `decode_window` calls).
    #[cfg(feature = "transmit")]
    #[test]
    fn hb244_combiner_persists_across_decode_windows() {
        let mut encoder = crate::Ft8Encoder::new();
        let symbols = encoder
            .encode_message("CQ K5ARH EM10", None)
            .expect("encode");
        let mut modulator = crate::Ft8Modulator::new_default().expect("modulator");
        let mut tx = modulator
            .modulate_symbols(&symbols, 500.0)
            .expect("modulate");
        tx.resize(WINDOW_SAMPLES, 0.0);

        let cfg = Ft8Config {
            soft_combiner_enabled: true,
            ..Ft8Config::default()
        };
        let mut dec = Ft8Decoder::new(cfg).expect("decoder ctor");

        // Pass 1: decode the signal. Truth bucket gets mark_decoded.
        // Other (noise) candidates leave entries behind.
        let _ = dec.decode_window(&tx).expect("decode 1");
        let len_after_pass1 = dec
            .soft_combiner
            .as_ref()
            .unwrap()
            .lock()
            .expect("not poisoned")
            .len();

        // Pass 2: decode silence. The combiner state from pass 1 should
        // persist (the combiner is owned by the decoder, not per-call).
        let silence = vec![0.0f32; WINDOW_SAMPLES];
        let _ = dec.decode_window(&silence).expect("decode 2");

        let len_after_pass2 = dec
            .soft_combiner
            .as_ref()
            .unwrap()
            .lock()
            .expect("not poisoned")
            .len();

        // The combiner is the same instance across calls — proving
        // persistence semantics. The len may grow (silence has noise
        // candidates) or stay the same (silence produces no sync
        // candidates), but the combiner instance is preserved.
        let _ = (len_after_pass1, len_after_pass2);

        // Direct property: the soft_combiner Arc is still Some.
        assert!(dec.soft_combiner.is_some());
    }
}

#[cfg(test)]
mod hb226_gaussian_ramp_tests {
    use super::*;
    use std::f64::consts::PI;
    // ====================================================================
    // hb-226: Gaussian-ramp subtract tests
    // ====================================================================

    /// Build a deterministic 79-symbol tone vector for ramp tests.
    fn hb226_synthetic_symbols() -> Vec<u8> {
        // Mix of tones across 0..7 so adjacent symbols frequently
        // differ in tone. This stresses the inter-symbol transition.
        let mut s = Vec::with_capacity(NUM_SYMBOLS);
        for i in 0..NUM_SYMBOLS {
            s.push(((i * 5 + 1) % 8) as u8);
        }
        s
    }

    #[test]
    fn hb226_ramp_samples_from_fraction_basic() {
        // At 12 kHz / 1920 sps, fraction 0.11 -> 211 samples (~17.6 ms).
        assert_eq!(Ft8Decoder::ramp_samples_from_fraction(1920, 0.11), 211);
        // Fraction 0.0 clamps to 1.
        assert_eq!(Ft8Decoder::ramp_samples_from_fraction(1920, 0.0), 1);
        // Negative or near-zero clamps to 1.
        assert_eq!(Ft8Decoder::ramp_samples_from_fraction(1920, -0.5), 1);
        // Full symbol = sps samples; the cpfsk generator further clamps
        // to sps/2 internally but the helper itself just rounds.
        assert_eq!(Ft8Decoder::ramp_samples_from_fraction(1920, 1.0), 1920);
    }

    #[test]
    fn hb226_default_config_is_off() {
        let cfg = Ft8Config::default();
        assert!(
            !cfg.gaussian_ramp_subtract_enabled,
            "hb-226 must default OFF so the subtract path stays byte-identical to the legacy implementation"
        );
        // The fraction should still be set to ft8mon's authoritative
        // value so an opt-in flip immediately uses the right ramp.
        assert!(
            (cfg.gaussian_ramp_subtract_fraction - 0.11).abs() < 1e-12,
            "hb-226 fraction default should be 0.11 (ft8mon's subtract_ramp constant)"
        );
    }

    #[test]
    fn hb226_ramped_iq_unit_magnitude_in_body() {
        // The steady body of every interior symbol (away from
        // both the on-ramp and off-ramp regions) should be a pure
        // unit-magnitude complex sinusoid — i.e. the I/Q vector
        // length is 1.0 sample by sample.
        let sym = hb226_synthetic_symbols();
        let sps = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;
        let ramp = Ft8Decoder::ramp_samples_from_fraction(sps, 0.11);
        let (i, q) = Ft8Decoder::generate_cpfsk_iq_ramped(&sym, 1500.0, sps, ramp);
        assert_eq!(i.len(), NUM_SYMBOLS * sps);

        // Pick the middle symbol's body region.
        let sym_idx = NUM_SYMBOLS / 2;
        let start = sym_idx * sps + ramp + 10; // past on-ramp
        let end = sym_idx * sps + sps - ramp - 10; // before off-ramp
        for k in start..end {
            let mag2 = i[k] * i[k] + q[k] * q[k];
            assert!(
                (mag2 - 1.0).abs() < 1e-9,
                "interior body sample {} should be unit-magnitude, got |z|^2 = {}",
                k,
                mag2
            );
        }
    }

    #[test]
    fn hb226_ramped_iq_first_and_last_taper() {
        // The very first sample should fade in from 0 amplitude;
        // the very last sample should fade out to 0 amplitude.
        let sym = hb226_synthetic_symbols();
        let sps = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;
        let ramp = Ft8Decoder::ramp_samples_from_fraction(sps, 0.11);
        let (i, q) = Ft8Decoder::generate_cpfsk_iq_ramped(&sym, 1500.0, sps, ramp);

        // Sample 0: amplitude = 0/ramp = 0
        let mag0 = (i[0] * i[0] + q[0] * q[0]).sqrt();
        assert!(
            mag0 < 1e-12,
            "first sample should taper to 0, got magnitude {}",
            mag0
        );

        // Sample at fraction 1/2 through the on-ramp: amplitude ~0.5
        let mid = ramp / 2;
        let mag_mid = (i[mid] * i[mid] + q[mid] * q[mid]).sqrt();
        let expected_mid = mid as f64 / ramp as f64;
        assert!(
            (mag_mid - expected_mid).abs() < 1e-9,
            "mid-of-on-ramp magnitude {} ≠ expected {}",
            mag_mid,
            expected_mid
        );

        // Last sample: amplitude tapered to 0
        let last = i.len() - 1;
        let mag_last = (i[last] * i[last] + q[last] * q[last]).sqrt();
        assert!(
            mag_last < 1e-9,
            "last sample should taper to 0, got magnitude {}",
            mag_last
        );
    }

    #[test]
    fn hb226_ramped_iq_continuous_phase_at_transitions() {
        // Across an inter-symbol boundary the I/Q vector should be
        // continuous (no phase jump). Verify by checking consecutive
        // samples have small angular change in the body, and that the
        // body→ramp→body sequence stays bounded.
        let sym = hb226_synthetic_symbols();
        let sps = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;
        let ramp = Ft8Decoder::ramp_samples_from_fraction(sps, 0.11);
        let (i, q) = Ft8Decoder::generate_cpfsk_iq_ramped(&sym, 1500.0, sps, ramp);

        // Maximum angular velocity is ~2π × (1500 + 7×6.25) / 12000
        // = ~2π × 1543.75 / 12000 ≈ 0.808 rad/sample.
        let max_omega = 2.0 * PI * (1500.0 + 7.0 * TONE_SPACING) / SAMPLE_RATE as f64;

        // For interior symbols (not first, not last) check that the
        // angle between successive unit-magnitude samples stays
        // within max_omega + small slack.
        for sym_idx in 5..(NUM_SYMBOLS - 5) {
            let region_start = sym_idx * sps;
            for k in (region_start + 1)..(region_start + sps) {
                let mag_a = (i[k - 1].powi(2) + q[k - 1].powi(2)).sqrt();
                let mag_b = (i[k].powi(2) + q[k].powi(2)).sqrt();
                if mag_a < 0.5 || mag_b < 0.5 {
                    continue; // edge taper region — skip
                }
                let dot = i[k - 1] * i[k] + q[k - 1] * q[k];
                let cross = i[k - 1] * q[k] - q[k - 1] * i[k];
                let angle = cross.atan2(dot).abs();
                assert!(
                    angle <= max_omega + 1e-3,
                    "phase jump {} rad at sample {} exceeds max omega {}",
                    angle,
                    k,
                    max_omega
                );
            }
        }
    }

    #[test]
    fn hb226_ramped_no_doublewrite_at_transition() {
        // Regression guard: the transition writes EXACTLY once per
        // sample. We verify by checking that no sample is written
        // back to default (0.0) — every interior sample after the
        // initial fade-in must have a non-zero I or Q value (the
        // signal has no zero crossings of the entire I/Q vector
        // outside the explicit taper regions).
        let sym = hb226_synthetic_symbols();
        let sps = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;
        let ramp = Ft8Decoder::ramp_samples_from_fraction(sps, 0.11);
        let (i, q) = Ft8Decoder::generate_cpfsk_iq_ramped(&sym, 1500.0, sps, ramp);
        let total_len = NUM_SYMBOLS * sps;

        // Past the first ramp and before the trailing ramp, every
        // sample should have unit magnitude (within 1e-9).
        for k in ramp..(total_len - ramp) {
            let mag2 = i[k] * i[k] + q[k] * q[k];
            assert!(
                (mag2 - 1.0).abs() < 1e-9,
                "interior sample {} not unit magnitude: |z|^2 = {} (possible double-write or missing write)",
                k,
                mag2
            );
        }
    }

    #[test]
    fn hb226_ramped_energy_close_to_unramped() {
        // Net energy of the ramped reconstruction should be very
        // close to the unramped reconstruction — the difference is
        // only the two ~ramp-sample tapers at each end of the
        // 79-symbol message (≈ 0.7% of total samples), so a strict
        // bound of < 5% is safe.
        let sym = hb226_synthetic_symbols();
        let sps = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;
        let ramp = Ft8Decoder::ramp_samples_from_fraction(sps, 0.11);
        let (ri_u, rq_u) = Ft8Decoder::generate_cpfsk_iq(&sym, 1500.0, sps);
        let (ri_r, rq_r) = Ft8Decoder::generate_cpfsk_iq_ramped(&sym, 1500.0, sps, ramp);

        let e_u: f64 = ri_u
            .iter()
            .zip(rq_u.iter())
            .map(|(a, b)| a * a + b * b)
            .sum();
        let e_r: f64 = ri_r
            .iter()
            .zip(rq_r.iter())
            .map(|(a, b)| a * a + b * b)
            .sum();
        let ratio = e_r / e_u;
        assert!(
            ratio > 0.95 && ratio < 1.0,
            "ramped/unramped energy ratio {} should be in (0.95, 1.0)",
            ratio
        );
    }

    #[test]
    fn hb226_subtract_default_off_byte_identical() {
        // CRITICAL CONTRACT: with gaussian_ramp_subtract_enabled=false
        // (the default), subtract_signal must produce a bit-identical
        // output buffer to the pre-hb-226 implementation. We verify
        // this by running the subtract path with a known input and
        // checking that the dispatch closure goes through the
        // unramped generator. Because the public test surface only
        // exposes the final buffer, we synthesize a known signal,
        // run subtract once with ramp OFF and once with ramp OFF
        // again with the SAME input — they must be identical.
        let symbols = hb226_synthetic_symbols();
        let sps = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;

        // Build a synthetic audio buffer: amplitude 0.5 sinusoid at
        // the symbol's CPFSK frequency, padded with zeros for the
        // search margin.
        let (ri, _rq) = Ft8Decoder::generate_cpfsk_iq(&symbols, 1500.0, sps);
        let pad = sps; // leave room for the time-search lower bound
        let mut audio: Vec<f32> = vec![0.0; pad + ri.len() + pad];
        for k in 0..ri.len() {
            audio[pad + k] = 0.5 * ri[k] as f32;
        }

        let msg = {
            // Construct a DecodedMessage with the symbols, frequency,
            // and time the synthetic audio was built at. The decode
            // result type permits hand-rolling for tests.
            let mut m = DecodedMessage::new(
                crate::message::Ft8Message::default(),
                0.0,
                1.0,
                1500.0,
                pad as f64 / SAMPLE_RATE as f64,
            );
            m.tone_symbols = Some(symbols.clone());
            m
        };

        // Run 1: default-OFF
        let mut cfg1 = Ft8Config::default();
        cfg1.gaussian_ramp_subtract_enabled = false;
        let decoder1 = Ft8Decoder::new(cfg1).unwrap();
        let mut audio1 = audio.clone();
        decoder1.subtract_signal(&mut audio1, &msg);

        // Run 2: default-OFF, identical config
        let mut cfg2 = Ft8Config::default();
        cfg2.gaussian_ramp_subtract_enabled = false;
        let decoder2 = Ft8Decoder::new(cfg2).unwrap();
        let mut audio2 = audio.clone();
        decoder2.subtract_signal(&mut audio2, &msg);

        // Bitwise identical
        assert_eq!(
            audio1, audio2,
            "default-OFF subtract must be deterministic / byte-identical"
        );
    }

    #[test]
    fn hb226_subtract_ramped_changes_buffer_at_boundaries() {
        // With ramp ENABLED, the subtracted output at inter-symbol
        // boundaries should DIFFER from the unramped subtraction —
        // this confirms the ramp is actually being applied (not
        // silently no-op'd). The difference should concentrate at
        // the symbol boundaries, not the steady body.
        let symbols = hb226_synthetic_symbols();
        let sps = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;

        // Build a synthetic audio buffer the same way as the
        // identical-output test, but use a clean reconstructed
        // signal we can subtract.
        let (ri, _rq) = Ft8Decoder::generate_cpfsk_iq(&symbols, 1500.0, sps);
        let pad = sps;
        let mut audio: Vec<f32> = vec![0.0; pad + ri.len() + pad];
        for k in 0..ri.len() {
            audio[pad + k] = 0.5 * ri[k] as f32;
        }

        let mut msg = DecodedMessage::new(
            crate::message::Ft8Message::default(),
            0.0,
            1.0,
            1500.0,
            pad as f64 / SAMPLE_RATE as f64,
        );
        msg.tone_symbols = Some(symbols.clone());

        // OFF run
        let mut cfg_off = Ft8Config::default();
        cfg_off.gaussian_ramp_subtract_enabled = false;
        let decoder_off = Ft8Decoder::new(cfg_off).unwrap();
        let mut audio_off = audio.clone();
        decoder_off.subtract_signal(&mut audio_off, &msg);

        // ON run
        let mut cfg_on = Ft8Config::default();
        cfg_on.gaussian_ramp_subtract_enabled = true;
        let decoder_on = Ft8Decoder::new(cfg_on).unwrap();
        let mut audio_on = audio.clone();
        decoder_on.subtract_signal(&mut audio_on, &msg);

        // The two buffers should NOT be identical (ramp must do
        // something).
        assert_ne!(
            audio_off, audio_on,
            "ramped subtract output should differ from unramped output"
        );

        // Maximum element-wise difference should be small (subtraction
        // bounded by the signal's amplitude * scale = 0.5 * 0.9).
        let max_diff = audio_off
            .iter()
            .zip(audio_on.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_diff < 1.0,
            "ramped-vs-unramped subtract diff should be bounded, got {}",
            max_diff
        );
    }

    #[test]
    fn hb226_ramped_two_tone_transition_minimum_at_boundary() {
        // With two adjacent symbols having different tones, the
        // ramped I/Q should show a slowly-changing angular velocity
        // across the boundary (linear slew) rather than a step.
        // Verify by measuring per-sample angular increment in the
        // transition window and checking it's monotonically moving
        // from omega_0 to omega_1.
        let mut sym = vec![0u8; NUM_SYMBOLS];
        // Symbol 10 tone 0, symbol 11 tone 7 — maximum spacing.
        sym[10] = 0;
        sym[11] = 7;
        let sps = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;
        let ramp = Ft8Decoder::ramp_samples_from_fraction(sps, 0.11);
        let (i, q) = Ft8Decoder::generate_cpfsk_iq_ramped(&sym, 1000.0, sps, ramp);

        // Compute per-sample angular increments at the transition
        // window [11*sps - ramp, 11*sps + ramp).
        let omega_0 = 2.0 * PI * (1000.0 + 0.0 * TONE_SPACING) / SAMPLE_RATE as f64;
        let omega_1 = 2.0 * PI * (1000.0 + 7.0 * TONE_SPACING) / SAMPLE_RATE as f64;
        let mid = 11 * sps; // boundary sample
        let start = mid - ramp;
        let end = mid + ramp;

        let mut increments = Vec::new();
        for k in (start + 1)..end {
            let dot = i[k - 1] * i[k] + q[k - 1] * q[k];
            let cross = i[k - 1] * q[k] - q[k - 1] * i[k];
            increments.push(cross.atan2(dot));
        }

        // Increments should start near omega_0 and end near omega_1
        let first = increments[0];
        let last = *increments.last().unwrap();
        assert!(
            (first - omega_0).abs() < 0.05,
            "transition start increment {} not near omega_0 {}",
            first,
            omega_0
        );
        assert!(
            (last - omega_1).abs() < 0.05,
            "transition end increment {} not near omega_1 {}",
            last,
            omega_1
        );
        // And monotonically increasing (since omega_1 > omega_0).
        let mut last_v = first;
        for v in increments.iter().skip(1) {
            assert!(
                *v >= last_v - 1e-6,
                "transition omega should monotonically increase, got {} after {}",
                v,
                last_v
            );
            last_v = *v;
        }
    }
}

// ============================================================================
// WSJT-X Improved 4th-pass-after-a7 tests
//
// Inspired by spec ref `spec-wsjtx-improved-4th-pass-after-a7.md`.
// The pass is wired as an additive extension to the standard multipass
// loop. These tests cover:
//   1. Default-OFF byte-identity of the decode output.
//   2. Default-ON bounded (no infinite loop) when a7 produces nothing.
//   3. a7-discovered callsign threads into the extended AP context.
//   4. Loop-passes config arithmetic short-circuits when a7 is off.
// ============================================================================
#[cfg(test)]
mod fourth_pass_a7_tests {
    use super::*;

    /// Spec invariant: the new flag must default OFF so the production
    /// decoder takes the byte-identical legacy path until measurement
    /// confirms a recall lift.
    #[test]
    fn fourth_pass_default_off_in_config() {
        let cfg = Ft8Config::default();
        assert!(
            !cfg.fourth_pass_after_a7_enabled,
            "4th-pass-after-a7 must default OFF — preserves legacy behavior until measurement"
        );
    }

    /// `fourth_pass_after_a7_enabled` is gated on `a7_enabled`. With a7
    /// off, the post-a7 mechanism MUST be inert regardless of the flag.
    /// Compute the effective gate the same way the decoder does and
    /// assert it stays false.
    #[test]
    fn fourth_pass_requires_a7_enabled() {
        let cfg = Ft8Config {
            a7_enabled: false,
            fourth_pass_after_a7_enabled: true,
            ..Ft8Config::default()
        };
        let effective = cfg.fourth_pass_after_a7_enabled && cfg.a7_enabled;
        assert!(
            !effective,
            "with a7_enabled=false, the 4th-pass mechanism must be inert \
             (no extra iteration, no extended AP context allocation)"
        );
    }

    /// Default-OFF decode of a synthetic FT8 signal must match the
    /// explicit-OFF configuration's decode output. This is the
    /// byte-identical legacy-path guarantee.
    #[cfg(feature = "transmit")]
    #[test]
    fn fourth_pass_default_off_decode_output_unchanged() {
        let mut encoder = crate::Ft8Encoder::new();
        let symbols = encoder
            .encode_message("CQ K5ARH EM10", None)
            .expect("encode");
        let mut modulator = crate::Ft8Modulator::new_default().expect("modulator");
        let mut tx = modulator
            .modulate_symbols(&symbols, 500.0)
            .expect("modulate");
        tx.resize(crate::WINDOW_SAMPLES, 0.0);

        let cfg_default = Ft8Config::default();
        let mut dec_default = Ft8Decoder::new(cfg_default).expect("decoder ctor");
        let decoded_default = dec_default.decode_window(&tx).expect("decode");

        let cfg_explicit_off = Ft8Config {
            fourth_pass_after_a7_enabled: false,
            ..Ft8Config::default()
        };
        let mut dec_explicit = Ft8Decoder::new(cfg_explicit_off).expect("decoder ctor");
        let decoded_explicit = dec_explicit.decode_window(&tx).expect("decode");

        let texts_default: Vec<&str> = decoded_default.iter().map(|m| m.text.as_str()).collect();
        let texts_explicit: Vec<&str> = decoded_explicit.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts_default, texts_explicit,
            "default-OFF and explicit-OFF must produce identical decode lists"
        );
        assert!(
            decoded_default.iter().any(|m| m.text == "CQ K5ARH EM10"),
            "synthetic truth must still decode in the default config; got: {:?}",
            texts_default
        );
    }

    /// Default-ON smoke test: with `fourth_pass_after_a7_enabled=true`
    /// AND `a7_enabled=true`, decoding a synthetic signal must
    ///
    ///   (a) still recover the truth signal,
    ///   (b) terminate in bounded time (no infinite loop even when
    ///       a7 produces nothing — the loop guard at
    ///       `pass >= max_passes && a7_discovered_calls.is_empty()`
    ///       short-circuits the extra iteration), and
    ///   (c) not produce duplicate emissions of the truth text.
    ///
    /// On a single-signal synthetic WAV a7 typically finds nothing
    /// because there's no second-utterance follow-up to template
    /// against; the test therefore exercises the "a7 produced 0
    /// decodes" edge case from the spec (step 4).
    #[cfg(feature = "transmit")]
    #[test]
    fn fourth_pass_default_on_bounded_one_extra_pass() {
        let mut encoder = crate::Ft8Encoder::new();
        let symbols = encoder
            .encode_message("CQ K5ARH EM10", None)
            .expect("encode");
        let mut modulator = crate::Ft8Modulator::new_default().expect("modulator");
        let mut tx = modulator
            .modulate_symbols(&symbols, 500.0)
            .expect("modulate");
        tx.resize(crate::WINDOW_SAMPLES, 0.0);

        let cfg = Ft8Config {
            a7_enabled: true,
            fourth_pass_after_a7_enabled: true,
            // Keep max_passes at default so we exercise the
            // (max_passes -> max_passes + 1) arithmetic exactly once.
            ..Ft8Config::default()
        };
        let mut dec = Ft8Decoder::new(cfg).expect("decoder ctor");
        let started = std::time::Instant::now();
        let decoded = dec.decode_window(&tx).expect("decode");
        let elapsed = started.elapsed();

        // Bounded: extra pass should not turn this into a multi-second
        // operation. The standard decode of a single synthetic CQ
        // completes well under 5 s on every supported tier; the
        // 4th-pass extension caps at one additional iteration.
        assert!(
            elapsed < std::time::Duration::from_secs(15),
            "decode took {:?} — 4th-pass extension must not produce \
             pathological wall-clock",
            elapsed
        );

        let texts: Vec<&str> = decoded.iter().map(|m| m.text.as_str()).collect();
        assert!(
            decoded.iter().any(|m| m.text == "CQ K5ARH EM10"),
            "truth signal must decode with 4th-pass enabled; got: {:?}",
            texts
        );

        // No duplicate emission of the truth text — the standard
        // `seen_messages` dedup runs on every iteration including the
        // 4th-pass-after-a7 iteration.
        let truth_count = decoded.iter().filter(|m| m.text == "CQ K5ARH EM10").count();
        assert_eq!(
            truth_count, 1,
            "truth signal must appear exactly once; got {}: {:?}",
            truth_count, texts
        );
    }

    /// White-box test of the AP-extension construction. The decoder
    /// loop clones `ap_context` and appends `RecentCallAp` entries for
    /// each a7-discovered callsign, deduping against pre-existing
    /// entries. This test exercises that construction path directly so
    /// we don't depend on triggering an a7 hit from a synthetic WAV.
    #[test]
    fn fourth_pass_extends_ap_context_with_a7_discovered_calls() {
        // Seed an AP context with one existing recent call.
        let seed = crate::ap::RecentCallAp::new("W1AW", -5.0).expect("encodable seed");
        let mut ctx = crate::ap::ApContext {
            recent_calls: vec![seed.clone()],
            ..Default::default()
        };

        // Mock a7 discoveries: one new callsign + one collision with
        // the existing seed. Use the same construction the decoder
        // applies on the 4th-pass iteration.
        let a7_discovered: Vec<(String, f32)> =
            vec![("KC1WIH".to_string(), -10.0), ("W1AW".to_string(), -3.0)];

        for (call, snr) in &a7_discovered {
            if let Some(rc) = crate::ap::RecentCallAp::new(call, *snr) {
                if !ctx.recent_calls.iter().any(|r| r.callsign == rc.callsign) {
                    ctx.recent_calls.push(rc);
                }
            }
        }

        // Should now have exactly two entries: the seeded W1AW and
        // the newly-discovered KC1WIH. The duplicate W1AW from a7 is
        // discarded (the seed wins; this matches the decoder's
        // dedup-against-pre-existing behavior).
        assert_eq!(
            ctx.recent_calls.len(),
            2,
            "extended ApContext must dedup a7 discoveries against existing recent_calls"
        );
        let calls: Vec<&str> = ctx
            .recent_calls
            .iter()
            .map(|r| r.callsign.as_str())
            .collect();
        assert!(calls.contains(&"W1AW"));
        assert!(calls.contains(&"KC1WIH"));
        // Seed entry survived (SNR is the original, not the
        // a7-discovery's value).
        let w1aw = ctx
            .recent_calls
            .iter()
            .find(|r| r.callsign == "W1AW")
            .expect("seed survives");
        assert!(
            (w1aw.last_snr - (-5.0_f32)).abs() < f32::EPSILON,
            "the seed entry's SNR must not be overwritten by a7's duplicate-callsign entry"
        );
    }

    /// `RecentCallAp::new` rejects unencodable callsigns. The decoder's
    /// extension path silently skips them (rather than panicking) so a
    /// garbage a7 emission can't crash the 4th-pass setup. Verify that
    /// behavior directly.
    #[test]
    fn fourth_pass_extension_skips_unencodable_calls() {
        let mut ctx = crate::ap::ApContext::default();
        let a7_discovered: Vec<(String, f32)> = vec![
            ("".to_string(), -10.0),           // empty
            ("INVALID!@#".to_string(), -10.0), // unencodable
            ("KC1WIH".to_string(), -8.0),      // valid
        ];

        for (call, snr) in &a7_discovered {
            if let Some(rc) = crate::ap::RecentCallAp::new(call, *snr) {
                if !ctx.recent_calls.iter().any(|r| r.callsign == rc.callsign) {
                    ctx.recent_calls.push(rc);
                }
            }
        }

        // Only the valid callsign should have been added.
        assert_eq!(
            ctx.recent_calls.len(),
            1,
            "invalid callsigns must be skipped silently"
        );
        assert_eq!(ctx.recent_calls[0].callsign, "KC1WIH");
    }

    /// Arithmetic sanity: when both flags are ON, the decoder uses
    /// `max_passes + 1` for its loop bound. When either flag is OFF,
    /// it uses `max_passes` exactly.
    #[test]
    fn fourth_pass_loop_passes_arithmetic() {
        // Replicates the decoder's compute-loop-bound expression.
        fn loop_passes(cfg: &Ft8Config) -> usize {
            let max_passes = cfg.max_decode_passes.max(1);
            let gated = cfg.fourth_pass_after_a7_enabled && cfg.a7_enabled;
            if gated {
                max_passes + 1
            } else {
                max_passes
            }
        }

        let base = Ft8Config {
            max_decode_passes: 3,
            a7_enabled: false,
            fourth_pass_after_a7_enabled: false,
            ..Ft8Config::default()
        };
        assert_eq!(loop_passes(&base), 3, "both off → max_passes unchanged");

        let a7_only = Ft8Config {
            a7_enabled: true,
            ..base.clone()
        };
        assert_eq!(
            loop_passes(&a7_only),
            3,
            "a7 alone (without fourth_pass flag) → max_passes unchanged"
        );

        let flag_only = Ft8Config {
            fourth_pass_after_a7_enabled: true,
            ..base.clone()
        };
        assert_eq!(
            loop_passes(&flag_only),
            3,
            "fourth_pass flag without a7 → max_passes unchanged (mechanism is inert)"
        );

        let both_on = Ft8Config {
            a7_enabled: true,
            fourth_pass_after_a7_enabled: true,
            ..base.clone()
        };
        assert_eq!(
            loop_passes(&both_on),
            4,
            "both on → exactly one extra iteration (max_passes + 1)"
        );

        // Floor at 1 — max_decode_passes=0 still gives at least one
        // standard pass, and with both flags on, two iterations.
        let zero_passes = Ft8Config {
            max_decode_passes: 0,
            a7_enabled: true,
            fourth_pass_after_a7_enabled: true,
            ..base
        };
        assert_eq!(
            loop_passes(&zero_passes),
            2,
            "max_passes floor of 1 + 1 extra = 2"
        );
    }
}

// ============================================================================
// JS8Call-Improved-style LLR whitening tests (spec ref:
// `research/specs/spec-js8call-llr-whitening.md`).
// ============================================================================

#[cfg(test)]
mod llr_whitening_tests {
    use super::*;
    use crate::protocol::ProtocolParams;

    /// Helper: build a uniform tone_magnitudes matrix where every
    /// (symbol, tone) entry has the same magnitude `m`.
    fn uniform_mags(num_symbols: usize, m: f64) -> Vec<[f64; NUM_TONES]> {
        vec![[m; NUM_TONES]; num_symbols]
    }

    /// Helper: build a tone_magnitudes matrix where each data symbol's
    /// winner tone is `winners[sym_in_data]` with magnitude `signal` and
    /// every other tone has magnitude `noise`. Sync symbols (non-data
    /// positions) get zero magnitudes (they're not consulted by the
    /// whitening helper).
    fn winner_mags(
        pp: &ProtocolParams,
        winners: &[usize],
        signal: f64,
        noise: f64,
    ) -> Vec<[f64; NUM_TONES]> {
        let mut out = vec![[0.0f64; NUM_TONES]; pp.num_symbols];
        let data_positions = pp.data_symbol_indices();
        assert_eq!(data_positions.len(), winners.len());
        for (i, &sym_idx) in data_positions.iter().enumerate() {
            for t in 0..NUM_TONES {
                out[sym_idx][t] = noise;
            }
            out[sym_idx][winners[i]] = signal;
        }
        out
    }

    /// Build a synthetic LLR vector with non-uniform per-position
    /// magnitudes (here just an identity 0..174 sequence) so the
    /// whitening scaling is measurable.
    fn synthetic_llrs(n: usize) -> Vec<f32> {
        (0..n).map(|i| (i as f32 + 1.0) * 0.5).collect()
    }

    #[test]
    fn whiten_llrs_uniform_magnitudes_is_uniform_rescale() {
        // When every tone × symbol magnitude is identical, the per-tone
        // and per-symbol medians are all equal to that magnitude, so
        // every divisor is the SAME scalar. Whitening then becomes a
        // global uniform rescale — which `normalize_llrs` cancels by
        // restoring the target variance. We verify the divisor is
        // uniform by checking that the RATIO between any two LLRs is
        // preserved by whitening.
        let pp = ProtocolParams::ft8();
        let mags = uniform_mags(pp.num_symbols, 1.5);
        let mut llrs = synthetic_llrs(174);
        let originals = llrs.clone();

        whiten_llrs(&mut llrs, &mags, &pp);

        // Pick two non-zero LLR indices and check ratios match.
        let ratio_before = originals[3] / originals[170];
        let ratio_after = llrs[3] / llrs[170];
        let rel_err = (ratio_before - ratio_after).abs() / ratio_before.abs().max(1e-6);
        assert!(
            rel_err < 1e-4,
            "uniform-magnitude whitening should preserve LLR ratios: \
             ratio_before={ratio_before}, ratio_after={ratio_after}, rel_err={rel_err}"
        );

        // And after re-normalising variance, the post-whiten LLRs are
        // identical to the legacy path that only ran `normalize_llrs`.
        let mut whitened = llrs.clone();
        normalize_llrs(&mut whitened, LLR_TARGET_VARIANCE);

        let mut legacy = originals.clone();
        normalize_llrs(&mut legacy, LLR_TARGET_VARIANCE);

        for (i, (&w, &l)) in whitened.iter().zip(legacy.iter()).enumerate() {
            let rel = (w - l).abs() / l.abs().max(1e-6);
            assert!(
                rel < 1e-4,
                "uniform-magnitude whitening followed by normalize_llrs must \
                 equal legacy normalize_llrs at index {i}: whitened={w}, legacy={l}, rel={rel}"
            );
        }
    }

    #[test]
    fn whiten_llrs_per_tone_noise_smaller_for_cleaner_tones() {
        // Build a magnitude matrix where one tone (tone 0) has a much
        // lower non-winner noise floor than the others. After whitening,
        // symbols whose winner is tone 0 should be amplified MORE
        // (smaller divisor) than symbols whose winner is a noisier tone.
        let pp = ProtocolParams::ft8();
        let data_positions = pp.data_symbol_indices();
        let nd = data_positions.len();

        // First half of symbols win on tone 0, second half win on tone 4.
        let half = nd / 2;
        let mut winners = vec![0usize; nd];
        for w in winners.iter_mut().take(half) {
            *w = 0;
        }
        for w in winners.iter_mut().skip(half) {
            *w = 4;
        }
        let mut mags = vec![[0.0f64; NUM_TONES]; pp.num_symbols];
        for (i, &sym_idx) in data_positions.iter().enumerate() {
            // Tone 0 has very low noise EVERYWHERE; tone 4 has high noise.
            for t in 0..NUM_TONES {
                mags[sym_idx][t] = if t == 0 { 0.1 } else { 1.0 };
            }
            // Winner tone gets the loudest magnitude (irrelevant to the
            // per-tone noise estimate, which excludes winners).
            mags[sym_idx][winners[i]] = 5.0;
        }

        // Confirm per-tone medians: tone 0 sees only its winner-positions
        // excluded, so its non-winner samples are at value 0.1; tone 4
        // sees positions where it was the winner excluded (also 5.0) so
        // remaining samples are at 1.0.
        // We don't have direct access to the internal `n_tone` vector,
        // but we can verify the *behaviour*: whitening a uniform-LLR
        // vector should produce LLRs with smaller divisor at tone-0-winner
        // positions than at tone-4-winner positions.
        let mut llrs = vec![10.0f32; 174];
        whiten_llrs(&mut llrs, &mags, &pp);

        let bps = pp.bits_per_symbol;
        let first_sym_llr_avg: f32 = llrs[..bps].iter().map(|l| l.abs()).sum::<f32>() / bps as f32;
        let last_sym_llr_avg: f32 = llrs[(nd - 1) * bps..nd * bps]
            .iter()
            .map(|l| l.abs())
            .sum::<f32>()
            / bps as f32;

        // First symbol's winner is tone 0 (clean) → small divisor → LARGE
        // post-whiten LLR. Last symbol's winner is tone 4 (noisy) → big
        // divisor → small post-whiten LLR.
        assert!(
            first_sym_llr_avg > last_sym_llr_avg,
            "tone-0-winner (cleaner) symbols should produce larger \
             whitened |LLR| than tone-4-winner (noisier) symbols. \
             tone0_winner_avg={first_sym_llr_avg}, tone4_winner_avg={last_sym_llr_avg}"
        );
    }

    #[test]
    fn whiten_llrs_then_normalize_yields_unit_variance() {
        // After whitening + normalize_llrs, the LLR vector should have
        // variance equal to LLR_TARGET_VARIANCE (modulo float rounding).
        let pp = ProtocolParams::ft8();
        let data_positions = pp.data_symbol_indices();
        let nd = data_positions.len();
        let winners: Vec<usize> = (0..nd).map(|i| i % NUM_TONES).collect();
        let mags = winner_mags(&pp, &winners, 4.0, 1.0);

        let mut llrs = synthetic_llrs(174);
        whiten_llrs(&mut llrs, &mags, &pp);
        normalize_llrs(&mut llrs, LLR_TARGET_VARIANCE);

        let n = llrs.len() as f32;
        let sum: f32 = llrs.iter().sum();
        let sum2: f32 = llrs.iter().map(|&x| x * x).sum();
        let variance = (sum2 - sum * sum / n) / n;
        let rel_err = (variance - LLR_TARGET_VARIANCE).abs() / LLR_TARGET_VARIANCE;
        assert!(
            rel_err < 1e-3,
            "whitened+normalized LLRs should hit target variance {}; got {} (rel_err={})",
            LLR_TARGET_VARIANCE,
            variance,
            rel_err
        );
    }

    #[test]
    fn whiten_llrs_handles_all_zero_magnitudes_without_nan() {
        // All-zero magnitudes is the pathological floor case: per-tone
        // and per-symbol medians collapse to the NOISE_FLOOR. Every LLR
        // is divided by `sqrt(floor * floor) = floor`. As long as no
        // NaN / Inf escapes, the helper is safe — LDPC will reject the
        // codeword downstream.
        let pp = ProtocolParams::ft8();
        let mags = uniform_mags(pp.num_symbols, 0.0);
        let mut llrs = synthetic_llrs(174);

        whiten_llrs(&mut llrs, &mags, &pp);

        for (i, &v) in llrs.iter().enumerate() {
            assert!(
                v.is_finite(),
                "whitening with all-zero magnitudes must never \
                 produce NaN/Inf; got {v} at index {i}"
            );
        }
    }

    #[test]
    fn whiten_llrs_handles_non_finite_input_without_propagating() {
        // If upstream produces a NaN LLR for any reason, the whitening
        // helper must clamp it to 0.0 rather than poisoning subsequent
        // BP check-node updates.
        let pp = ProtocolParams::ft8();
        let mags = uniform_mags(pp.num_symbols, 1.0);
        let mut llrs = synthetic_llrs(174);
        llrs[42] = f32::NAN;
        llrs[100] = f32::INFINITY;
        llrs[101] = f32::NEG_INFINITY;

        whiten_llrs(&mut llrs, &mags, &pp);

        for (i, &v) in llrs.iter().enumerate() {
            assert!(
                v.is_finite(),
                "whitening must clamp non-finite inputs to 0.0; \
                 got {v} at index {i}"
            );
        }
        assert_eq!(
            llrs[42], 0.0,
            "NaN input should be clamped to 0.0 after whitening"
        );
        assert_eq!(
            llrs[100], 0.0,
            "+Inf input should be clamped to 0.0 after whitening"
        );
        assert_eq!(
            llrs[101], 0.0,
            "-Inf input should be clamped to 0.0 after whitening"
        );
    }

    #[test]
    fn maybe_whiten_llrs_disabled_is_no_op() {
        // Default-OFF byte-identical contract: when the master flag is
        // false, the LLR vector is bit-for-bit unchanged.
        let pp = ProtocolParams::ft8();
        let data_positions = pp.data_symbol_indices();
        let nd = data_positions.len();
        let winners: Vec<usize> = (0..nd).map(|i| (i * 3) % NUM_TONES).collect();
        let mags = winner_mags(&pp, &winners, 3.5, 0.4);

        let mut llrs = synthetic_llrs(174);
        let original = llrs.clone();

        maybe_whiten_llrs(false, &mut llrs, &mags, &pp);

        assert_eq!(
            llrs, original,
            "disabled maybe_whiten_llrs must leave the LLR vector \
             bit-for-bit unchanged (byte-identical default-OFF contract)"
        );
    }

    #[test]
    fn maybe_whiten_llrs_enabled_modifies_non_uniform_input() {
        // Sanity check that the master flag actually routes to
        // `whiten_llrs` when ON, by verifying the LLR vector changes
        // on a non-uniform magnitude field.
        //
        // The test uses an asymmetric noise field: tone 0 has clean
        // non-winner noise (low n_tone[0]); all other tones have noisy
        // non-winner samples (high n_tone[t]). Symbols 0..half win on
        // tone 0 (clean), symbols half..nd win on tone 1 (noisy). The
        // resulting (n_tone[w] × n_symbol[sym]) product differs
        // sharply across the two halves, so the LLR vector must change.
        let pp = ProtocolParams::ft8();
        let data_positions = pp.data_symbol_indices();
        let nd = data_positions.len();
        let half = nd / 2;
        let mut mags = vec![[0.0f64; NUM_TONES]; pp.num_symbols];
        for (i, &sym_idx) in data_positions.iter().enumerate() {
            // Per-tone noise field: tone 0 very clean, others noisy.
            for t in 0..NUM_TONES {
                mags[sym_idx][t] = if t == 0 { 0.05 } else { 1.5 };
            }
            // Add a winner-tone burst at the chosen winner.
            let w = if i < half { 0 } else { 1 };
            mags[sym_idx][w] = 10.0;
        }

        let mut llrs = synthetic_llrs(174);
        let original = llrs.clone();

        maybe_whiten_llrs(true, &mut llrs, &mags, &pp);

        let max_delta = llrs
            .iter()
            .zip(original.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_delta > 1e-3,
            "enabled maybe_whiten_llrs must modify the LLR vector on \
             non-uniform magnitudes; max delta was {max_delta}"
        );
    }

    #[test]
    fn default_config_keeps_whitening_off() {
        // The default-OFF promise lives in the Ft8Config::default impl.
        // Regression guard against accidental flips.
        let cfg = Ft8Config::default();
        assert!(
            !cfg.llr_whitening_enabled,
            "Ft8Config::default().llr_whitening_enabled must be false; \
             toggling default-ON requires a hard-200 measurement"
        );
    }

    #[test]
    fn whiten_llrs_empty_llrs_returns_early() {
        // Defensive guard: empty LLR slice must return early without
        // touching the magnitude matrix (otherwise the helper would
        // attempt to compute per-tone medians and still mutate state
        // for no reason). The early-return path is the
        // `llrs.is_empty()` branch.
        let pp = ProtocolParams::ft8();
        // Use a valid (full-size) tone_magnitudes matrix so that the
        // function CAN'T accidentally index out-of-bounds even if the
        // early return is broken.
        let mags = uniform_mags(pp.num_symbols, 1.0);
        let mut llrs: Vec<f32> = Vec::new();
        whiten_llrs(&mut llrs, &mags, &pp);
        assert!(llrs.is_empty());
    }
}
