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
    DecodingMetrics, Ft8Error, Ft8Result, MessageHandler, NullMessageHandler, Protocol,
    NUM_SYMBOLS, NUM_TONES, SAMPLE_RATE, SYMBOL_DURATION, TONE_SPACING,
};
use bitvec::prelude::*;
use num_complex::Complex;
use rayon::prelude::*;
use rustfft::FftPlanner;
use std::collections::HashSet;
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
const LDPC_MAX_ITERATIONS: usize = 50;

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

    /// Enable multi-threading for parallel decoding
    pub enable_multithreading: bool,

    /// Maximum number of candidates to decode
    pub max_candidates: usize,

    /// LDPC decoder iterations
    pub ldpc_iterations: usize,

    /// Frequency search range (Hz)
    pub frequency_range: f64,

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
    /// candidate to be eligible for OSD fallback. Default 2 — hb-014
    /// (2026-05-23) swept {0..6} on curated-hard-200 and curated-hard-1000:
    /// recall is flat from 0 through 4, but novel-decode count (a proxy
    /// for false positives) grows monotonically with the gate. Tightening
    /// 4 → 2 cut FPs ~21% at zero recall cost and was ~26% faster.
    pub max_parity_errors_for_osd: usize,
}

impl Default for Ft8Config {
    fn default() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            protocol: Protocol::Ft8,
            enable_multithreading: true,
            max_candidates: MAX_DECODE_CANDIDATES,
            ldpc_iterations: LDPC_MAX_ITERATIONS,
            frequency_range: 200.0,
            time_range: 2.0,
            max_decode_passes: 1,
            osd_depth: Some(2),
            max_sync_candidates: MAX_SYNC_CANDIDATES,
            llr_target_variance: LLR_TARGET_VARIANCE,
            nms_enabled: false,
            nms_time_radius: NMS_TIME_RADIUS,
            nms_freq_radius: NMS_FREQ_RADIUS,
            min_sync_score: MIN_SYNC_SCORE,
            adaptive_ldpc_iters: false,
            block_score_rerank: true,
            max_parity_errors_for_osd: 2,
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
struct CostasCandidate {
    /// Time step in spectrogram (quarter-symbol units with TIME_OSR=2)
    time_step: usize,
    /// Base frequency bin in spectrogram (bin * 6.25 Hz)
    freq_bin: usize,
    /// Frequency sub-bin index (0..freq_osr-1)
    freq_sub: usize,
    /// Costas sync correlation score
    sync_score: f64,
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
        .with_max_parity_errors_for_osd(config.max_parity_errors_for_osd);

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
        })
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
        let start_time = Instant::now();
        self.message_handler.on_window_start(SystemTime::now());

        let min_samples = self.protocol_params.total_samples(SAMPLE_RATE);
        if samples.len() < min_samples {
            return Err(Ft8Error::InvalidWindowSize {
                expected: min_samples,
                actual: samples.len(),
            });
        }

        let max_passes = self.config.max_decode_passes.max(1);

        // Check whether AP is active (any known information available)
        let ap_active = ap_context.my_call.is_some() || ap_context.active_qso.is_some();

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

        for pass in 0..max_passes {
            if budget.expired() {
                info!(pass, "Decode budget expired, stopping early");
                break;
            }

            // Convert to f64 and normalize
            let audio = self.preprocess_audio(&residual_samples)?;

            // Step 1: Compute time-frequency spectrogram
            let spectrogram = self.compute_spectrogram(&audio)?;

            // Step 2: Find candidates via Costas sync pattern search
            let mut sync_candidates = self.costas_sync_search(&spectrogram)?;

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
                ap_context,
                ap_active,
                symbol_fft: &self.symbol_fft,
                symbol_window: &self.symbol_window,
                xor_sequence: self.protocol_params.xor_sequence,
                ldpc_iterations: self.config.ldpc_iterations,
                osd_depth: self.config.osd_depth,
                llr_target_variance: self.config.llr_target_variance,
                adaptive_ldpc_iters: self.config.adaptive_ldpc_iters,
                max_parity_errors_for_osd: self.config.max_parity_errors_for_osd,
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

            let pass_decoded: Vec<DecodedMessage> = sync_candidates
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
                            .with_max_parity_errors_for_osd(ctx.max_parity_errors_for_osd);
                        let ldpc_mid = LdpcDecoder::new_with_osd(iters_mid, osd_cfg)
                            .expect("LDPC decoder init failed")
                            .with_max_parity_errors_for_osd(ctx.max_parity_errors_for_osd);
                        let ldpc_high = LdpcDecoder::new_with_osd(iters_high, osd_cfg)
                            .expect("LDPC decoder init failed")
                            .with_max_parity_errors_for_osd(ctx.max_parity_errors_for_osd);
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

            // If no new messages decoded in this pass, stop iterating
            if pass_unique.is_empty() {
                break;
            }

            #[cfg(feature = "debug-decode")]
            eprintln!(
                "[decode pass {}] decoded {} new messages",
                pass,
                pass_unique.len()
            );

            // Subtract decoded signals from residual audio for next pass
            if pass + 1 < max_passes {
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

        // Fine frequency and time search to precisely match the actual signal.
        // The spectrogram has 3.125 Hz resolution, so sub-Hz precision is essential.
        // Frequency: +/-1.5 Hz in 0.5 Hz steps (7 freq trials)
        // Time: +/-480 samples (1/4 symbol) in 120-sample steps (9 time trials)
        let mut best_energy = 0.0f64;
        let mut best_freq = nominal_freq;
        let mut best_time = nominal_time;

        for di in -3i32..=3 {
            let try_freq = nominal_freq + di as f64 * 0.5;
            let (ri, rq) = Self::generate_cpfsk_iq(symbols, try_freq, sps);
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
        let (recon_i, recon_q) = Self::generate_cpfsk_iq(symbols, best_freq, sps);
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
                for fs in 0..freq_osr {
                    let mut row = Vec::with_capacity(num_bins);
                    for bin in 0..num_bins {
                        let src_bin = bin * freq_osr + fs;
                        if src_bin < nfft / 2 + 1 {
                            let mag2 = fft_buffer[src_bin].norm_sqr();
                            let db = 10.0 * (1e-12f64 + mag2).log10();
                            row.push(db);
                        } else {
                            row.push(-120.0);
                        }
                    }
                    sub_power.push(row);
                }
                power.push(sub_power);
            }
        }

        Ok(Spectrogram {
            power,
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
    fn costas_sync_search(&self, spectrogram: &Spectrogram) -> Ft8Result<Vec<CostasCandidate>> {
        let mut candidates = Vec::new();
        let pp = &self.protocol_params;

        // A full message occupies num_symbols * TIME_OSR time steps.
        let steps_per_symbol = TIME_OSR;
        let msg_span = pp.num_symbols * steps_per_symbol;
        let max_time_step = spectrogram.num_steps.saturating_sub(msg_span + 1);

        // Frequency range: need bins f0..f0+num_tones to all be valid
        let max_freq_bin = spectrogram.num_bins.saturating_sub(pp.num_tones);
        let max_freq_bin = max_freq_bin.min((4000.0 / pp.tone_spacing) as usize);

        for freq_sub in 0..spectrogram.freq_osr {
            for t0 in 0..=max_time_step {
                for f0 in MIN_FREQ_BIN..max_freq_bin {
                    let score = self.compute_costas_score(spectrogram, t0, f0, freq_sub);

                    if score > self.config.min_sync_score {
                        candidates.push(CostasCandidate {
                            time_step: t0,
                            freq_bin: f0,
                            freq_sub,
                            sync_score: score,
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

    /// Non-maximum suppression: remove weaker candidates near stronger ones
    fn nms_candidates(&self, candidates: &mut Vec<CostasCandidate>) {
        // candidates are already sorted by score (best first)
        let mut keep = vec![true; candidates.len()];

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
                time_step: candidate.time_step,
                freq_bin: candidate.freq_bin,
                freq_sub: trial_freq_sub,
                sync_score: candidate.sync_score,
            };
            let tone_magnitudes =
                self.extract_symbols_from_spectrogram(spectrogram, &trial_candidate);
            let base_llrs = self.compute_soft_llrs_db(&tone_magnitudes);

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
                time_step: candidate.time_step,
                freq_bin: candidate.freq_bin,
                freq_sub: trial_freq_sub,
                sync_score: candidate.sync_score,
            };
            let tone_magnitudes =
                self.extract_symbols_from_spectrogram(spectrogram, &trial_candidate);
            let mut llrs = self.compute_soft_llrs_db(&tone_magnitudes);
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
                // The finer time grid from TIME_OSR provides alignment via t0;
                // we only need 2 adjacent steps for the actual magnitude.
                let db_a = if t_base < spectrogram.num_steps {
                    spectrogram.power[t_base][fs][freq_bin]
                } else {
                    -120.0
                };
                let db_b = if t_base + 1 < spectrogram.num_steps {
                    spectrogram.power[t_base + 1][fs][freq_bin]
                } else {
                    -120.0
                };
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
    for &trial_freq_sub in &freq_sub_trials {
        let trial_candidate = CostasCandidate {
            time_step: candidate.time_step,
            freq_bin: candidate.freq_bin,
            freq_sub: trial_freq_sub,
            sync_score: candidate.sync_score,
        };
        let tone_magnitudes = par_extract_symbols_from_spectrogram(
            ctx.protocol_params,
            ctx.spectrogram,
            &trial_candidate,
        );
        let mut llrs = par_compute_soft_llrs_db(ctx.protocol_params, &tone_magnitudes);
        normalize_llrs(&mut llrs, ctx.llr_target_variance);

        if let Ok(corrected_bits) = ldpc.decode_soft(&llrs) {
            if par_verify_crc(&corrected_bits) {
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
            time_step: candidate.time_step,
            freq_bin: candidate.freq_bin,
            freq_sub: trial_freq_sub,
            sync_score: candidate.sync_score,
        };
        let tone_magnitudes = par_extract_symbols_from_spectrogram(
            ctx.protocol_params,
            ctx.spectrogram,
            &trial_candidate,
        );
        let base_llrs = par_compute_soft_llrs_db(ctx.protocol_params, &tone_magnitudes);

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

fn par_extract_symbols_from_spectrogram(
    pp: &ProtocolParams,
    spectrogram: &Spectrogram,
    candidate: &CostasCandidate,
) -> Vec<[f64; NUM_TONES]> {
    let t0 = candidate.time_step;
    let f0 = candidate.freq_bin;
    let fs = candidate.freq_sub;

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
            let db_a = if t_base < spectrogram.num_steps {
                spectrogram.power[t_base][fs][freq_bin]
            } else {
                -120.0
            };
            let db_b = if t_base + 1 < spectrogram.num_steps {
                spectrogram.power[t_base + 1][fs][freq_bin]
            } else {
                -120.0
            };
            mags[tone] = (db_a + db_b) / 2.0;
        }

        tone_magnitudes.push(mags);
    }

    tone_magnitudes
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

/// Estimate noise floor from power spectral density (median method)
fn estimate_noise_floor(psd: &[f64]) -> f64 {
    let mut sorted_psd = psd.to_vec();
    sorted_psd.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_idx = sorted_psd.len() / 2;
    sorted_psd[median_idx]
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
/// - Sum-product or min-sum belief propagation algorithm
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

        // BP did not converge — try OSD fallback if available.
        if let Some(ref osd) = self.osd {
            let llr_arr: &[f32; 174] = decoded_llrs[..174].try_into().unwrap();
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
                    let _ = trajectory;
                    None
                };

                if let Some(codeword) = osd.decode(llr_arr, neural_ordering.as_ref()) {
                    return Ok(codeword);
                }
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
        assert!(config.enable_multithreading);
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
    fn test_metrics_collection() {
        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();

        let samples = vec![0.0f32; WINDOW_SAMPLES];
        let _ = decoder.decode_window(&samples).unwrap();

        let metrics = decoder.get_last_metrics();
        assert_eq!(metrics.messages_decoded, 0);
        assert!(metrics.processing_time.as_millis() > 0);
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
            },
            CostasCandidate {
                time_step: 1,
                freq_bin: 240,
                freq_sub: 0,
                sync_score: 15.0,
            }, // near #0
            CostasCandidate {
                time_step: 0,
                freq_bin: 241,
                freq_sub: 0,
                sync_score: 12.0,
            }, // near #0
            CostasCandidate {
                time_step: 0,
                freq_bin: 300,
                freq_sub: 0,
                sync_score: 18.0,
            }, // far from #0
        ];

        decoder.nms_candidates(&mut candidates);

        // Should keep #0 (strongest) and #3 (far away), suppress #1 and #2
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].freq_bin, 240);
        assert_eq!(candidates[0].sync_score, 20.0);
        assert_eq!(candidates[1].freq_bin, 300);
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
}
