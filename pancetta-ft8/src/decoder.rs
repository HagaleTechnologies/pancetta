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
    DecodingMetrics, Ft8Error, Ft8Result, MessageHandler, NullMessageHandler, Protocol, NUM_TONES,
    NUM_SYMBOLS, SAMPLE_RATE, SYMBOL_DURATION, TONE_SPACING,
};
use bitvec::prelude::*;
use num_complex::Complex;
use rustfft::FftPlanner;
use std::collections::HashSet;
use std::time::{Instant, SystemTime};
use tracing::{debug, info};

// ============================================================================
// Constants
// ============================================================================

/// Maximum number of decode candidates to process
const MAX_DECODE_CANDIDATES: usize = 100;

/// Minimum SNR for attempting decode (dB)
const MIN_DECODE_SNR: f32 = -25.0;

/// LDPC decoder iterations
const LDPC_MAX_ITERATIONS: usize = 25;

/// FT8 Costas synchronization array
const COSTAS: [u8; 7] = [3, 1, 4, 0, 6, 5, 2];

/// Samples per FT8 symbol at 12 kHz (used only as fallback reference)
const SAMPLES_PER_SYMBOL: usize = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize; // 1920

/// Frequency oversampling rate (2 = sub-bin resolution)
const FREQ_OSR: usize = 2;

/// Time oversampling rate (2 = half-sub-symbol resolution, matching ft8_lib)
/// Each symbol occupies 2 * TIME_OSR time steps in the spectrogram.
const TIME_OSR: usize = 2;

/// Target LLR variance for normalization (matches ft8_lib's ftx_normalize_logl)
const LLR_TARGET_VARIANCE: f32 = 24.0;

/// Minimum Costas sync score to consider a candidate (dB difference, neighbor comparison)
const MIN_SYNC_SCORE: f64 = 3.0;

/// Maximum candidates from sync search before NMS
const MAX_SYNC_CANDIDATES: usize = 100;

/// Minimum frequency bin for FT8 search (16 bins × 6.25 Hz = 100 Hz)
const MIN_FREQ_BIN: usize = 16;

/// Non-maximum suppression radius in time steps (scaled with TIME_OSR)
const NMS_TIME_RADIUS: usize = 4 * TIME_OSR;

/// Non-maximum suppression radius in frequency bins
const NMS_FREQ_RADIUS: usize = 2;

// ============================================================================
// Decoder configuration
// ============================================================================

/// Decoder configuration for FT8/FT4/FT2 protocols
#[derive(Debug, Clone)]
pub struct Ft8Config {
    /// Sample rate (must be 12 kHz)
    pub sample_rate: u32,

    /// Protocol to decode (FT8, FT4, or FT2)
    pub protocol: Protocol,

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

    /// Maximum number of successive decoding passes (1 = no interference cancellation)
    pub max_decode_passes: usize,

    /// OSD depth (0, 1, or 2). Set to None to disable OSD. Default: Some(1).
    /// Note: OSD-2 (4,187 trials) has a high CRC-14 false positive rate without
    /// additional validation. OSD-1 (92 trials) is the safe default.
    pub osd_depth: Option<u8>,
}

impl Default for Ft8Config {
    fn default() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            protocol: Protocol::Ft8,
            enable_multithreading: true,
            max_candidates: MAX_DECODE_CANDIDATES,
            min_snr_db: MIN_DECODE_SNR,
            ldpc_iterations: LDPC_MAX_ITERATIONS,
            aggressive_decoding: false,
            frequency_range: 200.0,
            time_range: 2.0,
            max_decode_passes: 3,
            osd_depth: Some(2),
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
        )?;

        // Pre-compute FFT plan and Hann window for symbol extraction
        let sps = protocol_params.samples_per_symbol(SAMPLE_RATE);
        let mut planner = FftPlanner::<f64>::new();
        let symbol_fft = planner.plan_fft_forward(sps);
        let pi2 = 2.0 * std::f64::consts::PI;
        let symbol_window: Vec<f64> = (0..sps)
            .map(|i| 0.5 * (1.0 - (pi2 * i as f64 / (sps - 1) as f64).cos()))
            .collect();

        let symbol_fft_buffer = vec![Complex::new(0.0, 0.0); sps];

        Ok(Self {
            config,
            protocol_params,
            fft_processor,
            message_parser,
            ldpc_decoder,
            symbol_fft,
            symbol_window,
            symbol_fft_buffer,
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

        // Working copy of audio that we subtract decoded signals from
        let mut residual_samples: Vec<f32> = samples.to_vec();
        let mut all_decoded_messages: Vec<DecodedMessage> = Vec::new();
        let mut seen_messages: HashSet<String> = HashSet::new();
        let mut best_sync_score = 0.0f64;

        for pass in 0..max_passes {
            // Convert to f64 and normalize
            let audio = self.preprocess_audio(&residual_samples)?;

            // Step 1: Compute time-frequency spectrogram
            let spectrogram = self.compute_spectrogram(&audio)?;

            // Step 2: Find candidates via Costas sync pattern search
            let sync_candidates = self.costas_sync_search(&spectrogram)?;

            if pass == 0 {
                best_sync_score = sync_candidates.first().map(|c| c.sync_score).unwrap_or(0.0);
                info!(
                    candidates = sync_candidates.len(),
                    best_score = format!("{:.1}", best_sync_score),
                    spec_steps = spectrogram.num_steps,
                    spec_bins = spectrogram.num_bins,
                    "FT8 sync search pass 0"
                );
            }

            // Step 3: Decode each candidate
            let mut pass_decoded: Vec<DecodedMessage> = Vec::new();
            let _num_candidates = sync_candidates.len();
            let _best_score = sync_candidates.first().map(|c| c.sync_score).unwrap_or(0.0);
            #[cfg(feature = "debug-decode")]
            eprintln!(
                "[decode pass {}] {} sync candidates, best score={:.1}",
                pass, _num_candidates, _best_score
            );
            #[cfg(feature = "debug-decode")]
            for (i, c) in sync_candidates.iter().take(5).enumerate() {
                eprintln!(
                    "  [{}] t={} f={} score={:.1}",
                    i, c.time_step, c.freq_bin, c.sync_score
                );
            }
            for candidate in &sync_candidates {
                if all_decoded_messages.len() + pass_decoded.len() >= self.config.max_candidates {
                    break;
                }

                match self.decode_candidate(&audio, candidate, &spectrogram) {
                    Ok(Some(msg)) => {
                        // Deduplicate using HashSet for O(1) lookup
                        if seen_messages.insert(msg.text.clone()) {
                            pass_decoded.push(msg);
                        }
                    }
                    Ok(None) => {
                        #[cfg(feature = "debug-decode")]
                        eprintln!(
                            "  candidate t={} f={}: no decode",
                            candidate.time_step, candidate.freq_bin
                        );
                    }
                    Err(_e) => {
                        #[cfg(feature = "debug-decode")]
                        eprintln!(
                            "  candidate t={} f={}: error {}",
                            candidate.time_step, candidate.freq_bin, _e
                        );
                    }
                }
            }

            // If no new messages decoded in this pass, stop iterating
            if pass_decoded.is_empty() {
                break;
            }

            #[cfg(feature = "debug-decode")]
            eprintln!(
                "[decode pass {}] decoded {} new messages",
                pass,
                pass_decoded.len()
            );

            // Subtract decoded signals from residual audio for next pass
            if pass + 1 < max_passes {
                for msg in &pass_decoded {
                    self.subtract_signal(&mut residual_samples, msg);
                }
            }

            all_decoded_messages.extend(pass_decoded);
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
    fn generate_cpfsk_iq(
        symbols: &[u8],
        base_freq: f64,
        sps: usize,
    ) -> (Vec<f64>, Vec<f64>) {
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
                recon_i[start + i] = phase.sin();
                recon_q[start + i] = phase.cos();
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
        // Frequency: +/-3.0 Hz in 0.25 Hz steps (25 freq trials)
        // Time: +/-480 samples (1/4 symbol) in 120-sample steps (9 time trials)
        // Optimization: reuse the same I/Q signal across time offsets.
        let mut best_energy = 0.0f64;
        let mut best_freq = nominal_freq;
        let mut best_time = nominal_time;

        for di in -12i32..=12 {
            let try_freq = nominal_freq + di as f64 * 0.25;
            let (ri, rq) = Self::generate_cpfsk_iq(symbols, try_freq, sps);
            for dt in -4i32..=4 {
                let try_time = nominal_time + dt as isize * 120;
                let recon_start = try_time.max(0) as usize;
                let recon_offset = (recon_start as isize - try_time) as usize;
                let sig_len = (total_len.saturating_sub(recon_offset))
                    .min(audio.len().saturating_sub(recon_start));
                if sig_len == 0 { continue; }
                let energy = Self::correlation_energy(
                    audio, recon_start, &ri, &rq, recon_offset, sig_len,
                );
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
        let signal_len = (total_len.saturating_sub(recon_offset))
            .min(audio.len().saturating_sub(recon_start));

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
            let subtracted = amp_i * recon_i[recon_offset + i]
                + amp_q * recon_q[recon_offset + i];
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

    /// Compute power spectrogram of audio data with frequency oversampling.
    ///
    /// Uses FFT windows of 2× symbol period (3840 samples at 12 kHz) with
    /// freq_osr=2, giving 3.125 Hz frequency resolution. The FFT bins are
    /// then organized as freq_sub=0 (even bins: 0, 2, 4, ...) and
    /// freq_sub=1 (odd bins: 1, 3, 5, ...), where each sub-bin set has
    /// 6.25 Hz spacing. This matches ft8_lib's frequency oversampling approach.
    fn compute_spectrogram(&self, audio: &[f64]) -> Ft8Result<Spectrogram> {
        let pp = &self.protocol_params;
        let block_size = pp.samples_per_symbol(SAMPLE_RATE);
        let freq_osr = FREQ_OSR;
        let nfft = block_size * freq_osr;
        let step = block_size / (2 * TIME_OSR); // TIME_OSR=2 → quarter-symbol steps

        if audio.len() < block_size {
            return Err(Ft8Error::InsufficientData {
                needed: block_size,
                available: audio.len(),
            });
        }

        let num_steps = (audio.len() - block_size) / step + 1;
        // Number of frequency bins in 6.25 Hz units (= block_size/2 + 1)
        let num_bins = block_size / 2 + 1; // 961

        // FFT plan
        let mut planner = FftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(nfft);

        // Hann window of length nfft
        let window: Vec<f64> = (0..nfft)
            .map(|i| {
                0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / (nfft - 1) as f64).cos())
            })
            .collect();

        let mut power = Vec::with_capacity(num_steps);
        let mut fft_buffer = vec![Complex::new(0.0, 0.0); nfft];

        for t in 0..num_steps {
            let start = t * step;
            let end = (start + nfft).min(audio.len());

            // Apply window and load into FFT buffer, zero-pad if needed
            for i in 0..nfft {
                if start + i < end {
                    fft_buffer[i] = Complex::new(audio[start + i] * window[i], 0.0);
                } else {
                    fft_buffer[i] = Complex::new(0.0, 0.0);
                }
            }

            // Compute FFT in-place
            fft.process(&mut fft_buffer);

            // Organize into freq_osr sub-bins
            // FFT bin k corresponds to frequency k * (sample_rate / nfft)
            // = k * (12000 / 3840) = k * 3.125 Hz
            // In 6.25 Hz units: bin_6hz = k / freq_osr, freq_sub = k % freq_osr
            let mut sub_power = Vec::with_capacity(freq_osr);
            for fs in 0..freq_osr {
                let mut row = Vec::with_capacity(num_bins);
                for bin in 0..num_bins {
                    let src_bin = bin * freq_osr + fs;
                    if src_bin < nfft / 2 + 1 {
                        // Store log-magnitude (dB) like ft8_lib for neighbor scoring
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

        Ok(Spectrogram {
            power,
            num_steps,
            num_bins,
            freq_osr,
        })
    }

    // ========================================================================
    // Step 2: Costas sync search
    // ========================================================================

    /// Search for FT8 signals by correlating the Costas sync pattern
    /// against the spectrogram in 2D (time offset, frequency offset).
    ///
    /// The Costas array [3,1,4,0,6,5,2] appears at symbol positions 0-6,
    /// 36-42, and 72-78. For each candidate (t0, f0, freq_sub), we check
    /// all 21 Costas positions and score using neighbor comparison (ft8_lib style).
    /// With freq_osr=2, we search both even and odd frequency sub-bins.
    fn costas_sync_search(&self, spectrogram: &Spectrogram) -> Ft8Result<Vec<CostasCandidate>> {
        let mut candidates = Vec::new();
        let pp = &self.protocol_params;

        // A full message occupies num_symbols * (2 * TIME_OSR) time steps.
        let steps_per_symbol = 2 * TIME_OSR;
        let max_time_step = spectrogram.num_steps.saturating_sub(pp.num_symbols * steps_per_symbol + 1);

        // Frequency range: need bins f0..f0+num_tones to all be valid
        let max_freq_bin = spectrogram.num_bins.saturating_sub(pp.num_tones);
        let max_freq_bin = max_freq_bin.min((4000.0 / pp.tone_spacing) as usize);

        for freq_sub in 0..spectrogram.freq_osr {
            for t0 in 0..=max_time_step {
                for f0 in MIN_FREQ_BIN..max_freq_bin {
                    let score = self.compute_costas_score(spectrogram, t0, f0, freq_sub);

                    if score > MIN_SYNC_SCORE {
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
        candidates.truncate(MAX_SYNC_CANDIDATES);

        // Non-maximum suppression: remove weaker candidates near stronger ones
        self.nms_candidates(&mut candidates);

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
        let steps_per_symbol = 2 * TIME_OSR;
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
        let spec_step = sps / (2 * TIME_OSR);
        let coarse_offset = candidate.time_step * spec_step;

        // ---- Spectrogram-based symbol extraction: try both freq_sub values ----
        // The spectrogram uses a 3840-pt FFT (3.125 Hz resolution), which
        // avoids the spectral leakage of the 1920-pt independent FFT.
        // Signals on a bin boundary may decode better with the other sub-bin.
        let freq_sub_trials = [candidate.freq_sub, if candidate.freq_sub == 0 { 1 } else { 0 }];
        for &trial_freq_sub in &freq_sub_trials {
            let trial_candidate = CostasCandidate {
                time_step: candidate.time_step,
                freq_bin: candidate.freq_bin,
                freq_sub: trial_freq_sub,
                sync_score: candidate.sync_score,
            };
            let tone_magnitudes = self.extract_symbols_from_spectrogram(spectrogram, &trial_candidate);
            let mut llrs = self.compute_soft_llrs_db(&tone_magnitudes);
            normalize_llrs(&mut llrs);

            if let Ok(corrected_bits) = self.ldpc_decoder.decode_soft(&llrs) {
                if self.verify_crc(&corrected_bits) {
                    // CRC passed — compute frequency and time for the message
                    let sub_bin_offset =
                        trial_freq_sub as f64 * (tone_spacing / FREQ_OSR as f64);
                    let base_frequency =
                        candidate.freq_bin as f64 * tone_spacing + sub_bin_offset;
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
                    decoded_message.tone_symbols =
                        Some(Self::codeword_to_symbols(&corrected_bits));

                    #[cfg(feature = "debug-decode")]
                    eprintln!(
                        "    spectrogram path (freq_sub={}): CRC PASSED for t={} f={}",
                        trial_freq_sub, candidate.time_step, candidate.freq_bin
                    );

                    return Ok(Some(decoded_message));
                }
            }
        }

        // ---- Always try fine-timing FFT-based extraction too ----

        // Fine timing: search ±half symbol in eighth-symbol steps.
        // Finer time steps improve symbol extraction for signals not aligned to
        // the coarse Costas sync grid. 9 steps at 1/8 symbol = 240 samples each.
        let eighth_sym = (sps / 8) as isize;
        let time_deltas: [isize; 9] = [
            -4 * eighth_sym,
            -3 * eighth_sym,
            -2 * eighth_sym,
            -eighth_sym,
            0,
            eighth_sym,
            2 * eighth_sym,
            3 * eighth_sym,
            4 * eighth_sym,
        ];

        // Frequency refinement: try ±1 bin with half-bin sub-steps
        // This gives 5 frequency trials: -1, -0.5, 0, +0.5, +1
        // (in units of tone_spacing = 6.25 Hz, so steps are 3.125 Hz)
        let freq_offsets: [f64; 5] = [0.0, -0.5, 0.5, -1.0, 1.0];

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
                normalize_llrs(&mut llrs);

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
                decoded_message.tone_symbols =
                    Some(Self::codeword_to_symbols(&corrected_bits));

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

        let steps_per_symbol = 2 * TIME_OSR;

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
// Helper functions
// ============================================================================

/// Normalize LLR values to have target variance, matching ft8_lib's ftx_normalize_logl().
///
/// LDPC belief propagation is tuned for a specific LLR scale. This function
/// computes the variance of the 174 LLR values and scales them so the variance
/// equals LLR_TARGET_VARIANCE (24.0). This is critical for decoding weak signals.
fn normalize_llrs(llrs: &mut [f32]) {
    debug_assert_eq!(llrs.len(), 174);
    let n = llrs.len() as f32;
    let inv_n = 1.0 / n;

    let sum: f32 = llrs.iter().sum();
    let sum2: f32 = llrs.iter().map(|&x| x * x).sum();

    let variance = (sum2 - sum * sum * inv_n) * inv_n;

    if variance > 0.0 {
        let norm_factor = (LLR_TARGET_VARIANCE / variance).sqrt();
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
        })
    }

    fn new_with_osd(max_iterations: usize, osd_config: Option<OsdConfig>) -> Ft8Result<Self> {
        let mut decoder = Self::new(max_iterations)?;
        decoder.osd = osd_config.map(OsdDecoder::new);
        Ok(decoder)
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

        // Check if BP converged (syndrome = 0)
        let bp_converged = {
            let arr: &[f32; 174] = decoded_llrs[..174].try_into().unwrap();
            self.check_syndrome_fast(arr)
        };

        if bp_converged {
            return self.llrs_to_bits(&decoded_llrs);
        }

        // BP did not converge — try OSD fallback if available.
        // Only run OSD when BP was close to converging (few parity errors).
        // Without this gate, OSD-2's 4,187 trials per candidate × hundreds of
        // noise candidates produces excessive CRC-14 false positives (~1/16384
        // per trial).
        if let Some(ref osd) = self.osd {
            let llr_arr: &[f32; 174] = decoded_llrs[..174].try_into().unwrap();
            let parity_errors = self.count_parity_errors(llr_arr);

            // Threshold: only try OSD if BP nearly converged.
            // OSD-2 has 4187 trials × 1/16384 CRC false-positive rate per
            // candidate. Even with post-decode message validation, parity=5
            // admits too many false positives with plausible-looking callsign
            // structure. Parity=3 balances sensitivity and false-positive rate.
            const MAX_PARITY_ERRORS_FOR_OSD: usize = 3;

            if parity_errors <= MAX_PARITY_ERRORS_FOR_OSD {
                if let Some(codeword) = osd.decode(llr_arr) {
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
                    LdpcAlgorithm::MinSum { normalization_factor } => {
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
        let steps_per_symbol = 2 * TIME_OSR;
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
        normalize_llrs(&mut llrs);

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
        normalize_llrs(&mut llrs);
        let new_signs: Vec<bool> = llrs.iter().map(|&x| x > 0.0).collect();
        assert_eq!(signs, new_signs, "Normalization should preserve LLR signs");
    }

    #[test]
    fn test_llr_normalization_zero_variance() {
        // All same values: variance = 0, should not crash
        let mut llrs = vec![3.0f32; 174];
        normalize_llrs(&mut llrs);
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
        let symbols: Vec<u8> = (0..NUM_SYMBOLS).map(|i| {
            if i < 7 { COSTAS[i] }
            else if (36..43).contains(&i) { COSTAS[i - 36] }
            else if i >= 72 { COSTAS[i - 72] }
            else { 3 } // arbitrary data tone
        }).collect();

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
        };

        decoder.subtract_signal(&mut audio, &msg);

        // Measure energy after subtraction
        let energy_after: f64 = audio.iter().map(|&s| (s as f64) * (s as f64)).sum();

        let reduction = 1.0 - (energy_after / energy_before);
        eprintln!("Energy before: {:.6}, after: {:.6}, reduction: {:.1}%",
                  energy_before, energy_after, reduction * 100.0);

        // Should remove at least 70% of the energy
        assert!(reduction > 0.7,
                "Signal subtraction only removed {:.1}% of energy (expected >70%)",
                reduction * 100.0);
    }
}
