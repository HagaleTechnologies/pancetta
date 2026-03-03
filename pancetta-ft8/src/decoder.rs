//! Core FT8 decoder implementation
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
    Ft8Error, Ft8Result, DecodingMetrics, MessageHandler, NullMessageHandler,
    SAMPLE_RATE, SYMBOL_DURATION, WINDOW_SAMPLES, NUM_SYMBOLS, NUM_TONES, TONE_SPACING,
    message::{MessageParser, DecodedMessage, calculate_crc14, PAYLOAD_BITS, CRC_BITS},
    signal_processing::{FftProcessor, WindowFunction, BandpassFilter, SymbolCorrelator},
    sync::TimeSync,
};
use num_complex::Complex;
use rustfft::FftPlanner;
use std::time::{SystemTime, Instant};
use bumpalo::Bump;
use bitvec::prelude::*;

// ============================================================================
// Constants
// ============================================================================

/// Maximum number of decode candidates to process
const MAX_DECODE_CANDIDATES: usize = 50;

/// Minimum SNR for attempting decode (dB)
const MIN_DECODE_SNR: f32 = -25.0;

/// LDPC decoder iterations
const LDPC_MAX_ITERATIONS: usize = 100;

/// FT8 Costas synchronization array
const COSTAS: [u8; 7] = [3, 1, 4, 0, 6, 5, 2];

/// Samples per FT8 symbol at 12 kHz
const SAMPLES_PER_SYMBOL: usize = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize; // 1920

/// FFT size for spectrogram (one symbol period = exact 6.25 Hz resolution)
const SPEC_NFFT: usize = SAMPLES_PER_SYMBOL; // 1920

/// Spectrogram step size (half symbol for 2× oversampling)
const SPEC_STEP: usize = SAMPLES_PER_SYMBOL / 2; // 960

/// Minimum Costas sync score to consider a candidate
const MIN_SYNC_SCORE: f64 = 8.0;

/// Maximum candidates from sync search before NMS
const MAX_SYNC_CANDIDATES: usize = 200;

/// Minimum frequency bin for FT8 search (~200 Hz / 6.25 Hz)
const MIN_FREQ_BIN: usize = 32;

/// Non-maximum suppression radius in time steps (half-symbols)
const NMS_TIME_RADIUS: usize = 4;

/// Non-maximum suppression radius in frequency bins
const NMS_FREQ_RADIUS: usize = 2;

// ============================================================================
// Decoder configuration
// ============================================================================

/// FT8 decoder configuration
#[derive(Debug, Clone)]
pub struct Ft8Config {
    /// Sample rate (must be 12 kHz for FT8)
    pub sample_rate: u32,

    /// Enable multi-threading for parallel decoding
    pub enable_multithreading: bool,

    /// Maximum number of candidates to decode
    pub max_candidates: usize,

    /// Minimum SNR threshold for decoding
    pub min_snr_db: f32,

    /// LDPC decoder iterations
    pub ldpc_iterations: usize,

    /// Enable aggressive decoding (more CPU, better weak signal performance)
    pub aggressive_decoding: bool,

    /// Frequency search range (Hz)
    pub frequency_range: f64,

    /// Time search range (seconds)
    pub time_range: f64,
}

impl Default for Ft8Config {
    fn default() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            enable_multithreading: true,
            max_candidates: MAX_DECODE_CANDIDATES,
            min_snr_db: MIN_DECODE_SNR,
            ldpc_iterations: LDPC_MAX_ITERATIONS,
            aggressive_decoding: false,
            frequency_range: 200.0,
            time_range: 2.0,
        }
    }
}

// ============================================================================
// Internal data structures
// ============================================================================

/// Time-frequency spectrogram
struct Spectrogram {
    /// Power values [time_step][freq_bin]
    power: Vec<Vec<f64>>,
    /// Number of time steps
    num_steps: usize,
    /// Number of frequency bins (NFFT/2 + 1)
    num_bins: usize,
}

/// Costas sync search candidate
struct CostasCandidate {
    /// Time step in spectrogram (half-symbol units)
    time_step: usize,
    /// Base frequency bin in spectrogram (bin * 6.25 Hz)
    freq_bin: usize,
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

/// High-performance FT8 decoder
pub struct Ft8Decoder {
    /// Decoder configuration
    config: Ft8Config,

    /// FFT processor for waterfall display
    fft_processor: FftProcessor,

    /// Bandpass filter (kept for API compatibility)
    bandpass_filter: BandpassFilter,

    /// Symbol correlator (kept for API compatibility)
    symbol_correlator: SymbolCorrelator,

    /// Time synchronization engine (kept for API compatibility)
    time_sync: TimeSync,

    /// Message parser
    message_parser: MessageParser,

    /// LDPC decoder
    ldpc_decoder: LdpcDecoder,

    /// Allocator for zero-allocation hot path
    allocator: Bump,

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

    /// Create a new FT8 decoder with custom message handler
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

        let fft_processor = FftProcessor::new(4096, WindowFunction::Hann)?;
        let bandpass_filter = BandpassFilter::new(1500.0, 400.0, 65)?;
        let symbol_correlator = SymbolCorrelator::new()?;
        let time_sync = TimeSync::new()?;
        let message_parser = MessageParser::new();
        let ldpc_decoder = LdpcDecoder::new(config.ldpc_iterations)?;
        let allocator = Bump::with_capacity(1024 * 1024);

        Ok(Self {
            config,
            fft_processor,
            bandpass_filter,
            symbol_correlator,
            time_sync,
            message_parser,
            ldpc_decoder,
            allocator,
            message_handler,
            last_metrics: DecodingMetrics::default(),
        })
    }

    // ========================================================================
    // Main decode pipeline
    // ========================================================================

    /// Decode a 12.64-second window of audio samples
    pub fn decode_window(&mut self, samples: &[f32]) -> Ft8Result<Vec<DecodedMessage>> {
        let start_time = Instant::now();
        self.message_handler.on_window_start(SystemTime::now());

        if samples.len() < WINDOW_SAMPLES {
            return Err(Ft8Error::InvalidWindowSize {
                expected: WINDOW_SAMPLES,
                actual: samples.len(),
            });
        }

        self.allocator.reset();

        // Convert to f64 and normalize
        let audio = self.preprocess_audio(samples)?;

        // Step 1: Compute time-frequency spectrogram
        let spectrogram = self.compute_spectrogram(&audio)?;

        // Step 2: Find candidates via Costas sync pattern search
        let sync_candidates = self.costas_sync_search(&spectrogram)?;

        // Step 3: Decode each candidate
        let mut decoded_messages = Vec::new();
        let _num_candidates = sync_candidates.len();
        let _best_score = sync_candidates.first().map(|c| c.sync_score).unwrap_or(0.0);
        #[cfg(feature = "debug-decode")]
        eprintln!("[decode] {} sync candidates, best score={:.1}", _num_candidates, _best_score);
        #[cfg(feature = "debug-decode")]
        for (i, c) in sync_candidates.iter().take(5).enumerate() {
            eprintln!("  [{}] t={} f={} score={:.1}", i, c.time_step, c.freq_bin, c.sync_score);
        }
        for candidate in &sync_candidates {
            if decoded_messages.len() >= self.config.max_candidates {
                break;
            }

            match self.decode_candidate(&audio, candidate) {
                Ok(Some(msg)) => {
                    // Deduplicate by message text
                    if !decoded_messages.iter().any(|m: &DecodedMessage| m.text == msg.text) {
                        decoded_messages.push(msg);
                    }
                }
                Ok(None) => {
                    #[cfg(feature = "debug-decode")]
                    eprintln!("  candidate t={} f={}: no decode", candidate.time_step, candidate.freq_bin);
                }
                Err(_e) => {
                    #[cfg(feature = "debug-decode")]
                    eprintln!("  candidate t={} f={}: error {}", candidate.time_step, candidate.freq_bin, _e);
                }
            }
        }

        // Metrics
        let processing_time = start_time.elapsed();
        let best_sync = sync_candidates.first().map(|c| c.sync_score).unwrap_or(0.0);

        self.last_metrics = DecodingMetrics {
            messages_decoded: decoded_messages.len(),
            processing_time,
            average_snr: if decoded_messages.is_empty() {
                0.0
            } else {
                decoded_messages.iter().map(|m| m.snr_db).sum::<f32>()
                    / decoded_messages.len() as f32
            },
            peak_memory_bytes: self.allocator.allocated_bytes(),
            sync_quality: (best_sync / 30.0).min(1.0) as f32,
            timestamp: SystemTime::now(),
        };

        for message in &decoded_messages {
            self.message_handler.on_message_decoded(message, &self.last_metrics);
        }
        self.message_handler.on_window_complete(&self.last_metrics);

        Ok(decoded_messages)
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

        Ok(audio)
    }

    // ========================================================================
    // Step 1: Spectrogram
    // ========================================================================

    /// Compute power spectrogram of audio data.
    ///
    /// Uses FFT windows of exactly one symbol period (1920 samples at 12 kHz),
    /// giving 6.25 Hz frequency resolution — exactly one tone spacing per bin.
    /// Windows overlap by 50% (960-sample step).
    fn compute_spectrogram(&self, audio: &[f64]) -> Ft8Result<Spectrogram> {
        let nfft = SPEC_NFFT;
        let step = SPEC_STEP;

        if audio.len() < nfft {
            return Err(Ft8Error::InsufficientData {
                needed: nfft,
                available: audio.len(),
            });
        }

        let num_steps = (audio.len() - nfft) / step + 1;
        let num_bins = nfft / 2 + 1;

        // FFT plan
        let mut planner = FftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(nfft);

        // Hann window
        let window: Vec<f64> = (0..nfft)
            .map(|i| {
                0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / (nfft - 1) as f64).cos())
            })
            .collect();

        let mut power = Vec::with_capacity(num_steps);
        let mut fft_buffer = vec![Complex::new(0.0, 0.0); nfft];

        for t in 0..num_steps {
            let start = t * step;

            // Apply window and load into FFT buffer
            for i in 0..nfft {
                fft_buffer[i] = Complex::new(audio[start + i] * window[i], 0.0);
            }

            // Compute FFT in-place
            fft.process(&mut fft_buffer);

            // Compute power spectrum for positive frequencies
            let mut row = Vec::with_capacity(num_bins);
            for bin in 0..num_bins {
                row.push(fft_buffer[bin].norm_sqr());
            }
            power.push(row);
        }

        Ok(Spectrogram {
            power,
            num_steps,
            num_bins,
        })
    }

    // ========================================================================
    // Step 2: Costas sync search
    // ========================================================================

    /// Search for FT8 signals by correlating the Costas sync pattern
    /// against the spectrogram in 2D (time offset, frequency offset).
    ///
    /// The Costas array [3,1,4,0,6,5,2] appears at symbol positions 0-6,
    /// 36-42, and 72-78. For each candidate (t0, f0), we check all 21
    /// Costas positions and sum the signal-to-noise ratio at each one.
    fn costas_sync_search(&self, spectrogram: &Spectrogram) -> Ft8Result<Vec<CostasCandidate>> {
        let mut candidates = Vec::new();

        // A full 79-symbol message occupies 79 * 2 = 158 half-symbol steps.
        // The last Costas symbol is at position 78, which is step t0 + 2*78 = t0 + 156.
        let max_time_step = spectrogram.num_steps.saturating_sub(157);

        // Frequency range: need bins f0..f0+7 to all be valid
        let max_freq_bin = spectrogram.num_bins.saturating_sub(NUM_TONES);
        let max_freq_bin = max_freq_bin.min((4000.0 / TONE_SPACING) as usize);

        for t0 in 0..=max_time_step {
            for f0 in MIN_FREQ_BIN..max_freq_bin {
                let score = self.compute_costas_score(spectrogram, t0, f0);

                if score > MIN_SYNC_SCORE {
                    candidates.push(CostasCandidate {
                        time_step: t0,
                        freq_bin: f0,
                        sync_score: score,
                    });
                }
            }
        }

        // Sort by score (best first)
        candidates.sort_by(|a, b| {
            b.sync_score
                .partial_cmp(&a.sync_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.truncate(MAX_SYNC_CANDIDATES);

        // Non-maximum suppression: remove weaker candidates near stronger ones
        self.nms_candidates(&mut candidates);

        Ok(candidates)
    }

    /// Compute Costas sync score for a candidate at (t0, f0) in the spectrogram.
    ///
    /// For each of the 21 Costas sync positions, computes the log-power at
    /// the expected tone bin minus the average log-power of noise bins OUTSIDE
    /// the 8-tone signal range. Using external noise bins avoids contamination
    /// from spectral leakage between adjacent 6.25 Hz bins.
    fn compute_costas_score(&self, spec: &Spectrogram, t0: usize, f0: usize) -> f64 {
        let mut score = 0.0;
        let sync_group_starts: [usize; 3] = [0, 36, 72];

        // Noise estimation: use 8 bins below and 8 bins above the signal range.
        // Signal occupies bins f0..f0+7, noise uses f0-8..f0-1 and f0+8..f0+15.
        let noise_bins_below: Vec<usize> = (f0.saturating_sub(8)..f0)
            .filter(|&f| f < spec.num_bins)
            .collect();
        let noise_bins_above: Vec<usize> = (f0 + NUM_TONES..f0 + NUM_TONES + 8)
            .filter(|&f| f < spec.num_bins)
            .collect();

        for &group_start in &sync_group_starts {
            for j in 0..7 {
                let symbol_idx = group_start + j;
                // Each symbol occupies 2 time steps; use the first one
                let time_idx = t0 + symbol_idx * 2;

                if time_idx >= spec.num_steps {
                    return 0.0;
                }

                let expected_tone = COSTAS[j] as usize;
                let freq_idx = f0 + expected_tone;

                if freq_idx >= spec.num_bins {
                    return 0.0;
                }

                // Power at expected tone
                let signal_power = spec.power[time_idx][freq_idx];

                // Noise estimate from bins outside the 8-tone signal range
                let mut noise_power = 0.0;
                let mut noise_count = 0;
                for &f in noise_bins_below.iter().chain(noise_bins_above.iter()) {
                    noise_power += spec.power[time_idx][f];
                    noise_count += 1;
                }

                if noise_count > 0 {
                    noise_power /= noise_count as f64;
                }

                if noise_power > 0.0 {
                    score += (signal_power / noise_power).ln();
                } else if signal_power > 0.0 {
                    score += 10.0;
                }
            }
        }

        score
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
                let dt = (candidates[i].time_step as isize - candidates[j].time_step as isize).unsigned_abs();
                let df = (candidates[i].freq_bin as isize - candidates[j].freq_bin as isize).unsigned_abs();

                if dt <= NMS_TIME_RADIUS && df <= NMS_FREQ_RADIUS {
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

    /// Attempt to decode a single Costas sync candidate.
    ///
    /// Pipeline:
    /// 1. Fine timing search: refine coarse time offset (±half symbol, 5 steps)
    /// 2. Frequency refinement: try ±1 bin
    /// 3. Extract symbols with complex DFT
    /// 4. Compute soft LLRs
    /// 5. LDPC belief propagation
    /// 6. CRC-14 verification
    /// 7. Message parsing
    fn decode_candidate(
        &self,
        audio: &[f64],
        candidate: &CostasCandidate,
    ) -> Ft8Result<Option<DecodedMessage>> {
        let coarse_offset = candidate.time_step * SPEC_STEP;

        // Fine timing: search ±half symbol in sub-symbol steps.
        // The coarse sync has ±480 samples (half symbol) uncertainty.
        // Try 5 sub-offsets: -384, -192, 0, +192, +384 samples.
        let time_deltas: [isize; 5] = [-384, -192, 0, 192, 384];

        // Frequency refinement: try ±1 bin
        let freq_offsets: [isize; 3] = [0, -1, 1];

        // Find best (time_delta, freq_offset) by Costas correlation on extracted symbols
        let mut best_decode = None;

        for &dt in &time_deltas {
            let time_offset = coarse_offset as isize + dt;
            if time_offset < 0 {
                continue;
            }
            let time_offset_samples = time_offset as usize;

            for &df in &freq_offsets {
                let freq_bin = candidate.freq_bin as isize + df;
                if freq_bin < 0 {
                    continue;
                }
                let base_frequency = freq_bin as f64 * TONE_SPACING;

                let (_symbols, tone_magnitudes) = match self
                    .extract_symbols_complex(audio, time_offset_samples, base_frequency)
                {
                    Ok(result) => result,
                    Err(_) => continue,
                };

                let llrs = self.compute_soft_llrs(&tone_magnitudes);

                #[cfg(feature = "debug-decode")]
                {
                    let avg_abs_llr =
                        llrs.iter().map(|l| l.abs()).sum::<f32>() / llrs.len() as f32;
                    let saturated = llrs.iter().filter(|&&l| l.abs() >= 24.9).count();
                    eprintln!(
                        "    dt={:+4} df={:+2}: avg|LLR|={:.2}, sat={}/174",
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
                eprintln!("    dt={:+4} df={:+2}: CRC PASSED!", dt, df);

                let payload_bits = &corrected_bits[0..PAYLOAD_BITS];
                let ft8_message = self.message_parser.parse_payload(payload_bits)?;

                let snr_db = (candidate.sync_score / 21.0 * 4.343) as f32;
                let confidence = (candidate.sync_score / 30.0).min(1.0) as f32;

                let decoded_message = DecodedMessage::new(
                    ft8_message,
                    snr_db,
                    confidence,
                    base_frequency,
                    time_offset_samples as f64 / SAMPLE_RATE as f64,
                );

                best_decode = Some(decoded_message);
                return Ok(best_decode);
            }
        }

        Ok(best_decode)
    }

    // ========================================================================
    // Symbol extraction with complex DFT magnitude (Bug 1.2 fix)
    // ========================================================================

    /// Extract all 79 symbols from audio using complex DFT at each tone frequency.
    ///
    /// For each symbol position, computes the complex DFT (both cos and sin
    /// components) at each of the 8 tone frequencies, then uses the magnitude
    /// sqrt(real² + imag²) to determine the most likely tone. This is
    /// independent of the unknown carrier phase.
    ///
    /// Returns the hard-decision symbols AND the per-tone magnitude vectors
    /// (needed for soft LLR computation).
    fn extract_symbols_complex(
        &self,
        audio: &[f64],
        time_offset_samples: usize,
        base_frequency: f64,
    ) -> Ft8Result<(Vec<u8>, Vec<[f64; NUM_TONES]>)> {
        let end_sample = time_offset_samples + NUM_SYMBOLS * SAMPLES_PER_SYMBOL;
        if end_sample > audio.len() {
            return Err(Ft8Error::InsufficientData {
                needed: end_sample,
                available: audio.len(),
            });
        }

        let dt = 1.0 / SAMPLE_RATE as f64;
        let pi2 = 2.0 * std::f64::consts::PI;

        // Pre-compute Hann window for one symbol
        let window: Vec<f64> = (0..SAMPLES_PER_SYMBOL)
            .map(|i| {
                0.5 * (1.0
                    - (pi2 * i as f64 / (SAMPLES_PER_SYMBOL - 1) as f64).cos())
            })
            .collect();

        let mut symbols = Vec::with_capacity(NUM_SYMBOLS);
        let mut tone_magnitudes = Vec::with_capacity(NUM_SYMBOLS);

        for sym_idx in 0..NUM_SYMBOLS {
            let sym_start = time_offset_samples + sym_idx * SAMPLES_PER_SYMBOL;
            let symbol_audio = &audio[sym_start..sym_start + SAMPLES_PER_SYMBOL];

            let mut mags = [0.0f64; NUM_TONES];
            let mut best_tone = 0u8;
            let mut best_mag = 0.0;

            for tone in 0..NUM_TONES {
                let freq = base_frequency + tone as f64 * TONE_SPACING;

                // Complex DFT at this frequency with Hann window
                let mut real_sum = 0.0;
                let mut imag_sum = 0.0;

                for (i, &sample) in symbol_audio.iter().enumerate() {
                    let w = window[i];
                    let phase = pi2 * freq * i as f64 * dt;
                    real_sum += sample * w * phase.cos();
                    imag_sum += sample * w * phase.sin();
                }

                let magnitude = (real_sum * real_sum + imag_sum * imag_sum).sqrt();
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

    // ========================================================================
    // Soft LLR computation (Bug 1.3 fix)
    // ========================================================================

    /// Compute soft log-likelihood ratios from per-symbol tone magnitudes.
    ///
    /// For each of the 58 data symbols × 3 bits = 174 codeword bits,
    /// computes the LLR using the max-log approximation:
    ///
    ///   LLR(bit_k) ≈ (max mag²[t : bit_k(t)=0] - max mag²[t : bit_k(t)=1]) / (2σ²)
    ///
    /// where σ² is estimated per-symbol from the median tone magnitude.
    /// The Gray code mapping determines which tones correspond to bit=0 vs bit=1.
    fn compute_soft_llrs(&self, tone_magnitudes: &[[f64; NUM_TONES]]) -> Vec<f32> {
        let mut llrs = Vec::with_capacity(174);

        // Data symbol positions: 7..36 (29 symbols) and 43..72 (29 symbols)
        let data_positions: Vec<usize> = (7..36).chain(43..72).collect();

        for &sym_idx in &data_positions {
            let mags = &tone_magnitudes[sym_idx];

            // Per-symbol noise estimate: median of tone magnitude-squared values
            let mut mag_sq: Vec<f64> = mags.iter().map(|&m| m * m).collect();
            mag_sq.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let noise = mag_sq[mag_sq.len() / 2].max(1e-10);

            // For each of 3 bits (MSB first: bit2, bit1, bit0)
            for bit_pos in (0..3).rev() {
                let bit_mask = 1u8 << bit_pos;

                let mut max_0 = f64::NEG_INFINITY; // max mag² where bit=0
                let mut max_1 = f64::NEG_INFINITY; // max mag² where bit=1

                for tone in 0..NUM_TONES {
                    let binary = gray_to_binary(tone as u8);
                    let ms = mags[tone] * mags[tone];

                    if binary & bit_mask == 0 {
                        max_0 = max_0.max(ms);
                    } else {
                        max_1 = max_1.max(ms);
                    }
                }

                // LLR = (max_0 - max_1) / (2 * noise_variance)
                // Positive LLR → bit=0 more likely; negative → bit=1 more likely
                let llr = ((max_0 - max_1) / (2.0 * noise)) as f32;
                llrs.push(llr.clamp(-25.0, 25.0));
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
    // Demodulation (Gray code de-mapping)
    // ========================================================================

    /// Demodulate data symbols to bit sequence with Gray code de-mapping.
    ///
    /// FT8 layout: S7 D29 S7 D29 S7 (79 symbols total).
    /// Only the 58 data symbols (positions 7..36 and 43..72) are demodulated.
    /// Costas sync symbols at positions 0..7, 36..43, 72..79 are skipped.
    fn demodulate_symbols(&self, symbols: &[u8]) -> Ft8Result<BitVec> {
        if symbols.len() != NUM_SYMBOLS {
            return Err(Ft8Error::MessageDecodingError(format!(
                "Expected {} symbols, got {}",
                NUM_SYMBOLS,
                symbols.len()
            )));
        }

        let mut bits = BitVec::with_capacity(174);

        for i_tone in 0..NUM_SYMBOLS {
            let is_data = (7..36).contains(&i_tone) || (43..72).contains(&i_tone);
            if !is_data {
                continue;
            }

            let binary_value = gray_to_binary(symbols[i_tone]);
            bits.push((binary_value & 4) != 0);
            bits.push((binary_value & 2) != 0);
            bits.push((binary_value & 1) != 0);
        }

        Ok(bits)
    }

    // ========================================================================
    // Waterfall display
    // ========================================================================

    /// Generate waterfall display data
    pub fn generate_waterfall_data(&mut self, audio: &[f64]) -> Ft8Result<WaterfallData> {
        let window_size = 2048;
        let hop_size = window_size / 4;
        let num_windows = (audio.len().saturating_sub(window_size)) / hop_size + 1;

        let mut waterfall_data = WaterfallData {
            time_bins: Vec::new(),
            frequency_bins: Vec::new(),
            power_matrix: Vec::new(),
            min_power: f64::MAX,
            max_power: f64::MIN,
        };

        let freq_resolution = SAMPLE_RATE as f64 / window_size as f64;
        for i in 0..=window_size / 2 {
            let freq = i as f64 * freq_resolution;
            if freq >= 200.0 && freq <= 4000.0 {
                waterfall_data.frequency_bins.push(freq);
            }
        }

        for window_idx in 0..num_windows {
            let start = window_idx * hop_size;
            let end = (start + window_size).min(audio.len());

            if end - start < window_size {
                break;
            }

            let window = &audio[start..end];
            let psd = self.fft_processor.power_spectral_density(window)?;

            let mut window_powers = Vec::new();
            for (i, &power) in psd.iter().enumerate() {
                let freq = i as f64 * freq_resolution;
                if freq >= 200.0 && freq <= 4000.0 {
                    let power_db = 10.0 * power.log10();
                    window_powers.push(power_db);
                    waterfall_data.min_power = waterfall_data.min_power.min(power_db);
                    waterfall_data.max_power = waterfall_data.max_power.max(power_db);
                }
            }

            waterfall_data.power_matrix.push(window_powers);
            waterfall_data.time_bins.push(
                window_idx as f64 * hop_size as f64 / SAMPLE_RATE as f64,
            );
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
        self.time_sync.is_synchronized()
    }
}

// ============================================================================
// Helper functions
// ============================================================================

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

/// FT8 LDPC(174,91) decoder with belief propagation
///
/// Implements the LDPC decoder for FT8's (174,91) code:
/// - 91 information bits (77 payload + 14 CRC)
/// - 83 parity bits
/// - Min-sum belief propagation algorithm
struct LdpcDecoder {
    max_iterations: usize,
    /// Parity check matrix (83×174)
    parity_check_matrix: ParityCheckMatrix,
    /// Variable node degree (number of check nodes connected to each variable node)
    variable_degrees: Vec<usize>,
    /// Check node degree (number of variable nodes connected to each check node)
    check_degrees: Vec<usize>,
    /// Early termination threshold for syndrome check
    early_termination: bool,
    /// Min-sum normalization factor
    normalization_factor: f32,
}

impl LdpcDecoder {
    fn new(max_iterations: usize) -> Ft8Result<Self> {
        let parity_check_matrix = ParityCheckMatrix::new_ft8();

        let mut variable_degrees = vec![0; 174];
        let mut check_degrees = vec![0; 83];

        for check_idx in 0..83 {
            for var_idx in 0..174 {
                if parity_check_matrix.is_connected(check_idx, var_idx) {
                    variable_degrees[var_idx] += 1;
                    check_degrees[check_idx] += 1;
                }
            }
        }

        Ok(Self {
            max_iterations,
            parity_check_matrix,
            variable_degrees,
            check_degrees,
            early_termination: true,
            normalization_factor: 0.75,
        })
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

        let decoded_llrs = self.belief_propagation(llrs)?;
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

    /// Belief propagation algorithm using min-sum approximation
    fn belief_propagation(&self, channel_llrs: &[f32]) -> Ft8Result<Vec<f32>> {
        let mut variable_to_check = vec![vec![0.0f32; 174]; 83];
        let mut check_to_variable = vec![vec![0.0f32; 174]; 83];
        let mut output_llrs = channel_llrs.to_vec();

        // Initialize variable-to-check messages with channel LLRs
        for check_idx in 0..83 {
            for var_idx in 0..174 {
                if self.parity_check_matrix.is_connected(check_idx, var_idx) {
                    variable_to_check[check_idx][var_idx] = channel_llrs[var_idx];
                }
            }
        }

        for iteration in 0..self.max_iterations {
            // Check node update (min-sum algorithm)
            for check_idx in 0..83 {
                let connected_vars = self.parity_check_matrix.get_connected_variables(check_idx);

                for &var_idx in connected_vars {
                    let mut sign_product = 1.0f32;
                    let mut min_magnitude = f32::MAX;
                    let mut second_min_magnitude = f32::MAX;
                    let mut min_index = 0;

                    for &other_var in connected_vars {
                        if other_var != var_idx {
                            let msg = variable_to_check[check_idx][other_var];
                            sign_product *= msg.signum();

                            let magnitude = msg.abs();
                            if magnitude < min_magnitude {
                                second_min_magnitude = min_magnitude;
                                min_magnitude = magnitude;
                                min_index = other_var;
                            } else if magnitude < second_min_magnitude {
                                second_min_magnitude = magnitude;
                            }
                        }
                    }

                    let magnitude = if var_idx == min_index {
                        second_min_magnitude
                    } else {
                        min_magnitude
                    };

                    check_to_variable[check_idx][var_idx] =
                        sign_product * magnitude * self.normalization_factor;
                }
            }

            // Variable node update
            for var_idx in 0..174 {
                let connected_checks = self.parity_check_matrix.get_connected_checks(var_idx);

                output_llrs[var_idx] = channel_llrs[var_idx];
                for &check_idx in connected_checks {
                    output_llrs[var_idx] += check_to_variable[check_idx][var_idx];
                }

                for &check_idx in connected_checks {
                    variable_to_check[check_idx][var_idx] =
                        output_llrs[var_idx] - check_to_variable[check_idx][var_idx];
                }
            }

            // Early termination
            if self.early_termination && iteration > 0 && self.check_syndrome(&output_llrs) {
                return Ok(output_llrs);
            }
        }

        Ok(output_llrs)
    }

    /// Check if syndrome is zero (all parity checks satisfied)
    fn check_syndrome(&self, llrs: &[f32]) -> bool {
        for check_idx in 0..83 {
            let connected_vars = self.parity_check_matrix.get_connected_variables(check_idx);
            let mut parity = 0;

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
}

use crate::ldpc::{ParityCheckMatrix, gray_to_binary};

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use std::f64::consts::PI;

    #[test]
    fn test_ft8_config_default() {
        let config = Ft8Config::default();
        assert_eq!(config.sample_rate, SAMPLE_RATE);
        assert_eq!(config.max_candidates, MAX_DECODE_CANDIDATES);
        assert_eq!(config.min_snr_db, MIN_DECODE_SNR);
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
        assert_eq!(spec.power[0].len(), spec.num_bins);

        // The 1500 Hz tone should produce a peak at bin 1500/6.25 = 240
        let tone_bin = (1500.0 / TONE_SPACING) as usize;
        let mid_step = spec.num_steps / 2;

        // Power at tone bin should be much larger than at a random bin
        let signal_power = spec.power[mid_step][tone_bin];
        let noise_power = spec.power[mid_step][10]; // Low-frequency noise bin
        assert!(signal_power > noise_power * 100.0,
            "Signal power {:.2} should be >> noise power {:.2}", signal_power, noise_power);
    }

    #[test]
    fn test_costas_score_with_sync_signal() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();

        // Create a spectrogram where Costas tones are present at t0=0, f0=240
        let num_steps = 157;
        let num_bins = SPEC_NFFT / 2 + 1;
        let noise_level = 0.01;
        let signal_level = 1.0;
        let f0 = 240usize; // 1500 Hz

        let mut power = vec![vec![noise_level; num_bins]; num_steps];

        // Place Costas tones at the correct positions
        for &group_start in &[0usize, 36, 72] {
            for j in 0..7 {
                let sym = group_start + j;
                let time_idx = sym * 2;
                let tone = COSTAS[j] as usize;
                if time_idx < num_steps && f0 + tone < num_bins {
                    power[time_idx][f0 + tone] = signal_level;
                }
            }
        }

        let spec = Spectrogram { power, num_steps, num_bins };

        let score = decoder.compute_costas_score(&spec, 0, f0);
        assert!(score > MIN_SYNC_SCORE,
            "Costas score {:.2} should exceed threshold {:.2}", score, MIN_SYNC_SCORE);

        // Score at a wrong frequency should be much lower
        let wrong_score = decoder.compute_costas_score(&spec, 0, f0 + 20);
        assert!(score > wrong_score * 2.0,
            "Correct score {:.2} should be >> wrong score {:.2}", score, wrong_score);
    }

    #[test]
    fn test_complex_dft_tone_detection() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();

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
            assert_eq!(sym, target_tone,
                "Symbol {} detected tone {} instead of {}", i, sym, target_tone);
        }

        // Magnitude at target tone should dominate
        for (i, m) in mags.iter().enumerate() {
            assert!(m[target_tone as usize] > m[0] * 5.0,
                "Symbol {}: target mag {:.4} should dominate other mag {:.4}",
                i, m[target_tone as usize], m[0]);
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
            assert!(llr > 0.0,
                "LLR[{}] = {:.2} should be positive (bit=0 likely for tone 0)", i, llr);
        }
    }

    #[test]
    fn test_ldpc_decoder_creation() {
        let decoder = LdpcDecoder::new(50);
        assert!(decoder.is_ok());

        let ldpc = decoder.unwrap();
        assert_eq!(ldpc.max_iterations, 50);
        assert!(ldpc.early_termination);
        assert_eq!(ldpc.normalization_factor, 0.75);
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
        assert!(bits[0]);   // negative LLR → bit 1
        assert!(!bits[1]);  // positive LLR → bit 0
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
        assert!(waterfall.frequency_bins[0] >= 200.0);
        assert!(waterfall.frequency_bins.last().unwrap() <= &4000.0);
    }

    #[test]
    fn test_nms_suppression() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();

        let mut candidates = vec![
            CostasCandidate { time_step: 0, freq_bin: 240, sync_score: 20.0 },
            CostasCandidate { time_step: 1, freq_bin: 240, sync_score: 15.0 }, // near #0
            CostasCandidate { time_step: 0, freq_bin: 241, sync_score: 12.0 }, // near #0
            CostasCandidate { time_step: 0, freq_bin: 300, sync_score: 18.0 }, // far from #0
        ];

        decoder.nms_candidates(&mut candidates);

        // Should keep #0 (strongest) and #3 (far away), suppress #1 and #2
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].freq_bin, 240);
        assert_eq!(candidates[0].sync_score, 20.0);
        assert_eq!(candidates[1].freq_bin, 300);
    }
}
