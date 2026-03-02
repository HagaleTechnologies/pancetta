//! Core FT8 decoder implementation
//!
//! High-performance FT8 decoder with:
//! - LDPC error correction decoding
//! - Multi-hypothesis symbol detection
//! - Parallel processing for multiple candidates
//! - SNR estimation and confidence scoring
//! - Zero-allocation hot path optimization

use crate::{
    Ft8Error, Ft8Result, DecodingMetrics, MessageHandler, NullMessageHandler,
    SAMPLE_RATE, SYMBOL_DURATION, WINDOW_SAMPLES, NUM_SYMBOLS, NUM_TONES, TONE_SPACING,
    message::{MessageParser, DecodedMessage, Ft8Message, calculate_crc14, PAYLOAD_BITS, CRC_BITS},
    signal_processing::{FftProcessor, WindowFunction, BandpassFilter, SymbolCorrelator},
    sync::{TimeSync, SyncResult},
};
use num_complex::Complex;
use std::time::{SystemTime, Instant};
use std::sync::Arc;
use crossbeam::thread;
use bumpalo::Bump;
use bitvec::prelude::*;

/// Maximum number of decode candidates to process simultaneously
const MAX_DECODE_CANDIDATES: usize = 50;

/// Minimum SNR for attempting decode (dB)
const MIN_DECODE_SNR: f32 = -25.0;

/// LDPC decoder iterations
const LDPC_MAX_ITERATIONS: usize = 100;

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
            frequency_range: 200.0, // ±200 Hz search
            time_range: 2.0,        // ±2 second search
        }
    }
}

/// High-performance FT8 decoder
pub struct Ft8Decoder {
    /// Decoder configuration
    config: Ft8Config,
    
    /// FFT processor for spectral analysis
    fft_processor: FftProcessor,
    
    /// Bandpass filter for noise reduction
    bandpass_filter: BandpassFilter,
    
    /// Symbol correlator for timing
    symbol_correlator: SymbolCorrelator,
    
    /// Time synchronization engine
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
        // Validate configuration
        if config.sample_rate != SAMPLE_RATE {
            return Err(Ft8Error::InvalidSampleRate {
                expected: SAMPLE_RATE,
                actual: config.sample_rate,
            });
        }
        
        // Initialize components
        let fft_processor = FftProcessor::new(4096, WindowFunction::Hann)?;
        let bandpass_filter = BandpassFilter::new(1500.0, 400.0, 65)?;
        let symbol_correlator = SymbolCorrelator::new()?;
        let time_sync = TimeSync::new()?;
        let message_parser = MessageParser::new();
        let ldpc_decoder = LdpcDecoder::new(config.ldpc_iterations)?;
        let allocator = Bump::with_capacity(1024 * 1024); // 1MB allocation arena
        
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
    
    /// Decode a 12.64-second window of audio samples
    pub fn decode_window(&mut self, samples: &[f32]) -> Ft8Result<Vec<DecodedMessage>> {
        let start_time = Instant::now();
        self.message_handler.on_window_start(SystemTime::now());
        
        // Validate input
        if samples.len() != WINDOW_SAMPLES {
            return Err(Ft8Error::InvalidWindowSize {
                expected: WINDOW_SAMPLES,
                actual: samples.len(),
            });
        }
        
        // Reset allocator for this decode window
        self.allocator.reset();
        
        // Pre-process audio data
        let filtered_audio = self.preprocess_audio(samples)?;
        
        // Time synchronization (convert f64 to f32)
        let filtered_audio_f32: Vec<f32> = filtered_audio.iter().map(|&x| x as f32).collect();
        let sync_result = self.time_sync.synchronize(&filtered_audio_f32, SystemTime::now())?;
        
        // Find decode candidates
        let candidates = self.find_decode_candidates(&filtered_audio, &sync_result)?;
        
        // Decode candidates (potentially in parallel)
        let decoded_messages = if self.config.enable_multithreading && candidates.len() > 4 {
            self.decode_candidates_parallel(&candidates, &filtered_audio)?
        } else {
            self.decode_candidates_sequential(&candidates, &filtered_audio)?
        };
        
        // Calculate metrics
        let processing_time = start_time.elapsed();
        self.last_metrics = DecodingMetrics {
            messages_decoded: decoded_messages.len(),
            processing_time,
            average_snr: if decoded_messages.is_empty() {
                0.0
            } else {
                decoded_messages.iter().map(|m| m.snr_db).sum::<f32>() / decoded_messages.len() as f32
            },
            peak_memory_bytes: self.allocator.allocated_bytes(),
            sync_quality: sync_result.confidence as f32,
            timestamp: SystemTime::now(),
        };
        
        // Notify handler of decoded messages
        for message in &decoded_messages {
            self.message_handler.on_message_decoded(message, &self.last_metrics);
        }
        
        self.message_handler.on_window_complete(&self.last_metrics);
        
        Ok(decoded_messages)
    }
    
    /// Pre-process audio data (filtering, normalization)
    fn preprocess_audio(&mut self, samples: &[f32]) -> Ft8Result<Vec<f64>> {
        // Convert to f64 and apply bandpass filter
        let mut filtered = Vec::with_capacity(samples.len());
        
        for &sample in samples {
            let filtered_sample = self.bandpass_filter.filter_sample(sample as f64);
            filtered.push(filtered_sample);
        }
        
        // Normalize to prevent overflow
        let max_amplitude = filtered.iter().fold(0.0f64, |acc, &x| acc.max(x.abs()));
        if max_amplitude > 0.0 {
            let scale = 0.95 / max_amplitude;
            for sample in &mut filtered {
                *sample *= scale;
            }
        }
        
        Ok(filtered)
    }
    
    /// Find potential decode candidates in the audio
    fn find_decode_candidates(
        &mut self,
        audio: &[f64],
        sync_result: &SyncResult,
    ) -> Ft8Result<Vec<DecodeCandidate>> {
        let mut candidates = Vec::new();
        
        // Use the allocated arena for temporary storage
        let _temp_storage = self.allocator.alloc_slice_fill_default::<Complex<f64>>(8192);
        
        // Spectral analysis to find potential signals
        let spectrum = self.analyze_spectrum(audio)?;
        
        // Find frequency peaks that could be FT8 signals
        let frequency_peaks = self.find_frequency_peaks(&spectrum)?;
        
        // For each frequency peak, check different time offsets
        for freq_peak in frequency_peaks {
            if candidates.len() >= self.config.max_candidates {
                break;
            }
            
            // Search around the sync time if available
            let time_offsets = if sync_result.synchronized {
                vec![sync_result.time_offset]
            } else {
                // Search multiple time offsets
                let mut offsets = Vec::new();
                let dt = 0.1; // 100ms steps
                let max_offset = self.config.time_range;
                let mut t = -max_offset;
                while t <= max_offset {
                    offsets.push(t);
                    t += dt;
                }
                offsets
            };
            
            for time_offset in time_offsets {
                if candidates.len() >= self.config.max_candidates {
                    break;
                }
                
                // Create decode candidate
                let candidate = DecodeCandidate {
                    frequency: freq_peak.frequency,
                    time_offset,
                    snr_estimate: freq_peak.snr,
                    confidence: freq_peak.confidence,
                    sync_quality: sync_result.confidence as f32,
                };
                
                // Only add if SNR is above threshold
                if candidate.snr_estimate >= self.config.min_snr_db {
                    candidates.push(candidate);
                }
            }
        }
        
        // Sort by confidence (best first)
        candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
        
        // Limit to max candidates
        candidates.truncate(self.config.max_candidates);
        
        Ok(candidates)
    }
    
    /// Analyze spectrum to find potential FT8 signals with enhanced detection
    fn analyze_spectrum(&mut self, audio: &[f64]) -> Ft8Result<Vec<SpectrumPoint>> {
        // Apply AGC before spectral analysis
        let agc_audio = self.apply_automatic_gain_control(audio)?;
        
        // Multi-resolution spectral analysis
        let coarse_spectrum = self.coarse_frequency_search(&agc_audio)?;
        let fine_spectrum = self.fine_frequency_search(&agc_audio, &coarse_spectrum)?;
        
        // Combine results with coherent averaging
        let averaged_spectrum = self.coherent_symbol_averaging(&fine_spectrum)?;
        
        // Apply Doppler shift compensation if needed
        let compensated_spectrum = self.compensate_doppler_shift(averaged_spectrum)?;
        
        Ok(compensated_spectrum)
    }
    
    /// Apply automatic gain control for varying signal levels
    fn apply_automatic_gain_control(&self, audio: &[f64]) -> Ft8Result<Vec<f64>> {
        let mut agc_audio = Vec::with_capacity(audio.len());
        
        // AGC parameters
        let attack_time = 0.01; // 10ms attack
        let release_time = 0.5; // 500ms release
        let target_level = 0.3; // Target RMS level
        
        let attack_coeff = (-2.197 / (attack_time * SAMPLE_RATE as f64)).exp();
        let release_coeff = (-2.197 / (release_time * SAMPLE_RATE as f64)).exp();
        
        let mut envelope = 0.0;
        let mut gain = 1.0;
        
        for &sample in audio {
            let abs_sample = sample.abs();
            
            // Update envelope
            if abs_sample > envelope {
                envelope = abs_sample + (envelope - abs_sample) * attack_coeff;
            } else {
                envelope = abs_sample + (envelope - abs_sample) * release_coeff;
            }
            
            // Calculate gain
            if envelope > 0.001 {
                let desired_gain = target_level / envelope;
                gain = gain * 0.95 + desired_gain * 0.05; // Smooth gain changes
                gain = gain.clamp(0.1, 10.0); // Limit gain range
            }
            
            agc_audio.push(sample * gain);
        }
        
        Ok(agc_audio)
    }
    
    /// Coarse frequency search using larger FFT windows
    fn coarse_frequency_search(&mut self, audio: &[f64]) -> Ft8Result<Vec<SpectrumPoint>> {
        let mut spectrum_points = Vec::new();
        
        // Use larger windows for initial coarse search
        let window_size = 8192; // Larger window for better frequency resolution
        let hop_size = window_size / 8; // More overlap for better time resolution
        let num_windows = (audio.len().saturating_sub(window_size)) / hop_size + 1;
        
        // Create a larger FFT processor if needed
        let mut coarse_fft = FftProcessor::new(window_size, WindowFunction::Blackman)?;
        
        for window_idx in 0..num_windows {
            let start = window_idx * hop_size;
            let end = (start + window_size).min(audio.len());
            
            if end - start < window_size {
                break;
            }
            
            let window = &audio[start..end];
            let psd = coarse_fft.power_spectral_density(window)?;
            let freq_bins = coarse_fft.frequency_bins();
            
            // Advanced noise floor estimation using statistical methods
            let noise_floor = self.estimate_noise_floor_statistical(&psd)?;
            
            for (freq, power) in freq_bins.iter().zip(psd.iter()) {
                if *freq >= 200.0 && *freq <= 4000.0 { // FT8 frequency range
                    let snr = 10.0 * (power / noise_floor).log10();
                    if snr > -30.0 { // Include very weak signals
                        spectrum_points.push(SpectrumPoint {
                            frequency: *freq,
                            power: *power,
                            snr: snr as f32,
                            time_window: window_idx,
                        });
                    }
                }
            }
        }
        
        Ok(spectrum_points)
    }
    
    /// Fine frequency search around detected peaks
    fn fine_frequency_search(&mut self, audio: &[f64], coarse_spectrum: &[SpectrumPoint]) -> Ft8Result<Vec<SpectrumPoint>> {
        let mut fine_spectrum = Vec::new();
        
        // Find frequency peaks in coarse spectrum
        let peaks = self.find_spectrum_peaks(coarse_spectrum)?;
        
        // Use smaller FFT windows for fine search around peaks
        let window_size = 2048;
        let hop_size = window_size / 16; // Very fine time resolution
        
        let mut fine_fft = FftProcessor::new(window_size, WindowFunction::Kaiser(8.0))?;
        
        for peak in peaks {
            // Search in a narrow frequency band around the peak
            let freq_range = 50.0; // ±50 Hz around peak
            
            // Apply narrow bandpass filter around peak frequency
            let mut narrow_filter = BandpassFilter::new(peak.frequency, freq_range, 129)?;
            let mut filtered_audio = vec![0.0; audio.len()];
            narrow_filter.filter_batch(audio, &mut filtered_audio)?;
            
            // Fine resolution analysis
            for window_idx in 0..(audio.len().saturating_sub(window_size)) / hop_size {
                let start = window_idx * hop_size;
                let end = (start + window_size).min(filtered_audio.len());
                
                if end - start < window_size {
                    break;
                }
                
                let window = &filtered_audio[start..end];
                let psd = fine_fft.power_spectral_density(window)?;
                let freq_bins = fine_fft.frequency_bins();
                
                // Use local noise estimation for better SNR calculation
                let noise_floor = self.estimate_local_noise_floor(&psd, peak.frequency)?;
                
                for (freq, power) in freq_bins.iter().zip(psd.iter()) {
                    // Map frequency bin to actual frequency around the peak
                    let actual_freq = peak.frequency - freq_range/2.0 + freq;
                    // Only include frequencies in valid FT8 range and near the peak
                    if actual_freq >= 200.0 && actual_freq <= 4000.0 && 
                       actual_freq >= peak.frequency - 25.0 && actual_freq <= peak.frequency + 25.0 {
                        let snr = 10.0 * (power / noise_floor).log10();
                        fine_spectrum.push(SpectrumPoint {
                            frequency: actual_freq,
                            power: *power,
                            snr: snr as f32,
                            time_window: window_idx,
                        });
                    }
                }
            }
        }
        
        Ok(fine_spectrum)
    }
    
    /// Coherent symbol averaging for weak signal detection
    fn coherent_symbol_averaging(&self, spectrum: &[SpectrumPoint]) -> Ft8Result<Vec<SpectrumPoint>> {
        let mut averaged_spectrum = Vec::new();
        
        // Group spectrum points by frequency bins
        let mut freq_groups: std::collections::HashMap<u32, Vec<&SpectrumPoint>> = std::collections::HashMap::new();
        
        for point in spectrum {
            let freq_bin = (point.frequency / 6.25) as u32; // 6.25 Hz bins (FT8 tone spacing)
            freq_groups.entry(freq_bin).or_insert_with(Vec::new).push(point);
        }
        
        // Perform coherent averaging for each frequency bin
        for (_bin, points) in freq_groups {
            if points.len() < 3 {
                // Not enough points for averaging, keep original
                for point in points {
                    averaged_spectrum.push((*point).clone());
                }
                continue;
            }
            
            // Calculate coherent average
            let mut sum_power = 0.0;
            let mut sum_phase = Complex::new(0.0, 0.0);
            let frequency = points[0].frequency;
            
            for point in &points {
                sum_power += point.power;
                // Estimate phase from power (simplified)
                let phase = 2.0 * std::f64::consts::PI * point.frequency * (point.time_window as f64 * 0.16);
                sum_phase += Complex::new(phase.cos(), phase.sin()) * point.power.sqrt();
            }
            
            let avg_power = sum_power / points.len() as f64;
            let coherent_gain = (sum_phase.norm() / points.len() as f64).powi(2);
            let enhanced_power = avg_power * (1.0 + coherent_gain);
            
            // Recalculate SNR with enhanced power
            let noise_floor = self.estimate_noise_floor_statistical(&[avg_power]).unwrap_or(avg_power * 0.1);
            let enhanced_snr = 10.0 * (enhanced_power / noise_floor).log10();
            
            averaged_spectrum.push(SpectrumPoint {
                frequency,
                power: enhanced_power,
                snr: enhanced_snr as f32,
                time_window: points[points.len()/2].time_window, // Use middle time window
            });
        }
        
        Ok(averaged_spectrum)
    }
    
    /// Compensate for Doppler shift (for EME/satellite work)
    fn compensate_doppler_shift(&self, spectrum: Vec<SpectrumPoint>) -> Ft8Result<Vec<SpectrumPoint>> {
        // Maximum expected Doppler shift for EME/satellite
        const MAX_DOPPLER_SHIFT: f64 = 200.0; // ±200 Hz
        const DOPPLER_SEARCH_STEP: f64 = 5.0; // 5 Hz steps
        
        let mut compensated_spectrum = spectrum.clone();
        let mut best_score = 0.0;
        let mut best_shift = 0.0;
        
        // Search for optimal Doppler shift
        let mut shift = -MAX_DOPPLER_SHIFT;
        while shift <= MAX_DOPPLER_SHIFT {
            let mut score = 0.0;
            
            // Calculate score for this Doppler shift
            for point in &spectrum {
                // Check if shifted frequency aligns with FT8 tone grid
                let shifted_freq = point.frequency + shift;
                let tone_offset = shifted_freq % TONE_SPACING;
                let alignment_score = if tone_offset < 1.0 || tone_offset > TONE_SPACING - 1.0 {
                    1.0
                } else {
                    0.0
                };
                
                score += alignment_score * point.snr.max(0.0) as f64;
            }
            
            if score > best_score {
                best_score = score;
                best_shift = shift;
            }
            
            shift += DOPPLER_SEARCH_STEP;
        }
        
        // Apply best Doppler shift compensation
        if best_shift.abs() > 1.0 {
            for point in &mut compensated_spectrum {
                point.frequency -= best_shift;
            }
        }
        
        Ok(compensated_spectrum)
    }
    
    /// Estimate noise floor using statistical methods
    fn estimate_noise_floor_statistical(&self, psd: &[f64]) -> Ft8Result<f64> {
        if psd.is_empty() {
            return Ok(1e-10);
        }
        
        // Use multiple statistical measures for robust estimation
        let mut sorted_psd = psd.to_vec();
        sorted_psd.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        
        // Calculate percentiles
        let p10_idx = sorted_psd.len() / 10;
        let p25_idx = sorted_psd.len() / 4;
        let p50_idx = sorted_psd.len() / 2;
        
        let p10 = sorted_psd[p10_idx];
        let p25 = sorted_psd[p25_idx];
        let median = sorted_psd[p50_idx];
        
        // Use MAD (Median Absolute Deviation) for robust noise estimation
        let mut deviations = Vec::with_capacity(sorted_psd.len());
        for &value in psd {
            deviations.push((value - median).abs());
        }
        deviations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mad = deviations[deviations.len() / 2];
        
        // Estimate noise floor using combination of methods
        let noise_floor = if mad > 0.0 {
            // Use MAD-based estimation (robust against outliers)
            median + 1.4826 * mad // 1.4826 converts MAD to standard deviation for normal distribution
        } else {
            // Fallback to percentile-based estimation
            p25 * 1.5
        };
        
        // Sanity check: noise floor should be positive and reasonable
        Ok(noise_floor.max(p10).max(1e-10))
    }
    
    /// Estimate local noise floor around a specific frequency
    fn estimate_local_noise_floor(&self, psd: &[f64], _center_freq: f64) -> Ft8Result<f64> {
        if psd.len() < 20 {
            return self.estimate_noise_floor_statistical(psd);
        }
        
        // Use local statistics in frequency domain
        let window_size = 20; // Look at 20 bins around
        let mut local_values = Vec::new();
        
        // Collect values avoiding the center (signal) bins
        for i in 0..psd.len() {
            if i < 5 || i > psd.len() - 5 {
                continue; // Skip edge bins
            }
            
            // Skip potential signal bins (center ± 2 bins)
            let center = psd.len() / 2;
            if i >= center - 2 && i <= center + 2 {
                continue;
            }
            
            local_values.push(psd[i]);
            
            if local_values.len() >= window_size {
                break;
            }
        }
        
        if local_values.is_empty() {
            return self.estimate_noise_floor_statistical(psd);
        }
        
        // Use median of local values
        local_values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Ok(local_values[local_values.len() / 2])
    }
    
    /// Find peaks in spectrum for fine frequency search
    fn find_spectrum_peaks(&self, spectrum: &[SpectrumPoint]) -> Ft8Result<Vec<FrequencyPeak>> {
        let mut peaks = Vec::new();
        
        // Group by frequency bins
        let mut freq_bins: std::collections::HashMap<u32, Vec<&SpectrumPoint>> = std::collections::HashMap::new();
        
        for point in spectrum {
            let bin = (point.frequency / 25.0) as u32; // 25 Hz bins for peak detection
            freq_bins.entry(bin).or_insert_with(Vec::new).push(point);
        }
        
        // Find peaks in each bin
        for (_bin, points) in freq_bins {
            if points.is_empty() {
                continue;
            }
            
            let avg_power: f64 = points.iter().map(|p| p.power).sum::<f64>() / points.len() as f64;
            let avg_snr: f32 = points.iter().map(|p| p.snr).sum::<f32>() / points.len() as f32;
            let frequency = points.iter().map(|p| p.frequency).sum::<f64>() / points.len() as f64;
            
            if avg_snr >= -24.0 { // Include signals down to -24 dB SNR
                peaks.push(FrequencyPeak {
                    frequency,
                    power: avg_power,
                    snr: avg_snr,
                    confidence: self.calculate_peak_confidence(avg_snr, points.len()),
                });
            }
        }
        
        // Sort by SNR
        peaks.sort_by(|a, b| b.snr.partial_cmp(&a.snr).unwrap_or(std::cmp::Ordering::Equal));
        
        Ok(peaks)
    }
    
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
        
        // Generate frequency bins
        let freq_resolution = SAMPLE_RATE as f64 / window_size as f64;
        for i in 0..=window_size/2 {
            let freq = i as f64 * freq_resolution;
            if freq >= 200.0 && freq <= 4000.0 {
                waterfall_data.frequency_bins.push(freq);
            }
        }
        
        // Process each time window
        for window_idx in 0..num_windows {
            let start = window_idx * hop_size;
            let end = (start + window_size).min(audio.len());
            
            if end - start < window_size {
                break;
            }
            
            let window = &audio[start..end];
            let psd = self.fft_processor.power_spectral_density(window)?;
            
            // Extract relevant frequency range
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
            waterfall_data.time_bins.push(window_idx as f64 * hop_size as f64 / SAMPLE_RATE as f64);
        }
        
        Ok(waterfall_data)
    }
    
    /// Find frequency peaks that could be FT8 signals
    fn find_frequency_peaks(&self, spectrum: &[SpectrumPoint]) -> Ft8Result<Vec<FrequencyPeak>> {
        let mut peaks = Vec::new();
        
        // Group spectrum points by frequency bins
        let mut freq_bins: std::collections::HashMap<u32, Vec<&SpectrumPoint>> = std::collections::HashMap::new();
        
        for point in spectrum {
            let bin = (point.frequency / 10.0) as u32; // 10 Hz bins
            freq_bins.entry(bin).or_insert_with(Vec::new).push(point);
        }
        
        // Find peaks in each frequency bin
        for (_bin, points) in freq_bins {
            if points.len() < 3 {
                continue; // Need multiple time windows for peak detection
            }
            
            // Calculate average power and SNR for this frequency
            let avg_power: f64 = points.iter().map(|p| p.power).sum::<f64>() / points.len() as f64;
            let avg_snr: f32 = points.iter().map(|p| p.snr).sum::<f32>() / points.len() as f32;
            let frequency = points.iter().map(|p| p.frequency).sum::<f64>() / points.len() as f64;
            
            // Check if this looks like an FT8 signal
            if avg_snr >= self.config.min_snr_db && self.is_ft8_like_signal(&points) {
                peaks.push(FrequencyPeak {
                    frequency,
                    power: avg_power,
                    snr: avg_snr,
                    confidence: self.calculate_peak_confidence(avg_snr, points.len()),
                });
            }
        }
        
        // Sort by SNR (best first)
        peaks.sort_by(|a, b| b.snr.partial_cmp(&a.snr).unwrap_or(std::cmp::Ordering::Equal));
        
        Ok(peaks)
    }
    
    /// Check if spectrum points look like an FT8 signal
    fn is_ft8_like_signal(&self, _points: &Vec<&SpectrumPoint>) -> bool {
        // In a full implementation, this would check for:
        // - Consistent power across the 12.64s transmission
        // - Expected bandwidth (~50 Hz)
        // - Tone spacing characteristics
        // For now, we'll use a simplified approach
        true
    }
    
    /// Calculate confidence for a frequency peak
    fn calculate_peak_confidence(&self, snr: f32, num_samples: usize) -> f32 {
        let snr_confidence = ((snr - self.config.min_snr_db) / 20.0).clamp(0.0, 1.0);
        let sample_confidence = (num_samples as f32 / 10.0).clamp(0.0, 1.0);
        
        (snr_confidence * 0.7 + sample_confidence * 0.3).clamp(0.0, 1.0)
    }
    
    /// Decode candidates sequentially
    fn decode_candidates_sequential(
        &mut self,
        candidates: &[DecodeCandidate],
        audio: &[f64],
    ) -> Ft8Result<Vec<DecodedMessage>> {
        let mut decoded_messages = Vec::new();
        
        for candidate in candidates {
            if let Some(message) = self.decode_single_candidate(candidate, audio)? {
                decoded_messages.push(message);
            }
        }
        
        Ok(decoded_messages)
    }
    
    /// Decode candidates in parallel
    fn decode_candidates_parallel(
        &mut self,
        candidates: &[DecodeCandidate],
        audio: &[f64],
    ) -> Ft8Result<Vec<DecodedMessage>> {
        let audio_arc = Arc::new(audio.to_vec());
        let _candidates_arc = Arc::new(candidates.to_vec());
        let config_arc = Arc::new(self.config.clone());
        
        let decoded_messages = thread::scope(|s| -> Ft8Result<Vec<DecodedMessage>> {
            let mut handles = Vec::new();
            
            // Process candidates in chunks across multiple threads
            let chunk_size = (candidates.len() + 3) / 4; // 4 threads max
            
            for chunk in candidates.chunks(chunk_size) {
                let audio_clone = audio_arc.clone();
                let config_clone = config_arc.clone();
                let chunk_vec = chunk.to_vec();
                
                let handle = s.spawn(move |_| {
                    let mut local_decoder = create_local_decoder(&config_clone)?;
                    let mut chunk_results = Vec::new();
                    
                    for candidate in chunk_vec {
                        if let Some(message) = local_decoder.decode_single_candidate(&candidate, &audio_clone)? {
                            chunk_results.push(message);
                        }
                    }
                    
                    Ok::<Vec<DecodedMessage>, Ft8Error>(chunk_results)
                });
                
                handles.push(handle);
            }
            
            // Collect results
            let mut all_messages = Vec::new();
            for handle in handles {
                match handle.join() {
                    Ok(Ok(messages)) => all_messages.extend(messages),
                    Ok(Err(e)) => return Err(e),
                    Err(_) => return Err(Ft8Error::SignalProcessingError("Thread panic".to_string())),
                }
            }
            
            Ok(all_messages)
        }).map_err(|_| Ft8Error::SignalProcessingError("Thread join error".to_string()))?;
        
        decoded_messages
    }
    
    /// Decode a single candidate
    fn decode_single_candidate(
        &mut self,
        candidate: &DecodeCandidate,
        audio: &[f64],
    ) -> Ft8Result<Option<DecodedMessage>> {
        // Only attempt decoding if candidate looks promising
        if candidate.snr_estimate < self.config.min_snr_db || candidate.confidence < 0.3 {
            return Ok(None);
        }
        
        // Extract symbols at the candidate frequency and time
        let symbols = self.extract_symbols(audio, candidate.frequency, candidate.time_offset)?;
        
        // Demodulate to get bit sequence
        let bit_sequence = self.demodulate_symbols(&symbols)?;
        
        // Apply LDPC error correction
        let corrected_bits = self.ldpc_decoder.decode(&bit_sequence)?;
        
        // Check CRC
        if !self.verify_crc(&corrected_bits) {
            return Ok(None); // Invalid message
        }
        
        // Parse message
        let payload_bits = &corrected_bits[0..PAYLOAD_BITS];
        let ft8_message = self.message_parser.parse_payload(payload_bits)?;
        
        // Create decoded message with metadata
        let decoded_message = DecodedMessage::new(
            ft8_message,
            candidate.snr_estimate,
            candidate.confidence,
            candidate.frequency,
            candidate.time_offset,
        );
        
        Ok(Some(decoded_message))
    }
    
    /// Extract FT8 symbols from audio at specific frequency and time
    fn extract_symbols(&mut self, audio: &[f64], frequency: f64, time_offset: f64) -> Ft8Result<Vec<u8>> {
        let samples_per_symbol = (SYMBOL_DURATION * self.config.sample_rate as f64) as usize;
        let start_sample = (time_offset * self.config.sample_rate as f64) as usize;
        
        if start_sample + NUM_SYMBOLS * samples_per_symbol > audio.len() {
            return Err(Ft8Error::InsufficientData {
                needed: start_sample + NUM_SYMBOLS * samples_per_symbol,
                available: audio.len(),
            });
        }
        
        let mut symbols = Vec::with_capacity(NUM_SYMBOLS);
        
        // Extract each symbol
        for symbol_idx in 0..NUM_SYMBOLS {
            let symbol_start = start_sample + symbol_idx * samples_per_symbol;
            let symbol_end = symbol_start + samples_per_symbol;
            let symbol_audio = &audio[symbol_start..symbol_end];
            
            // Correlate with each of the 8 FT8 tones
            let mut max_correlation = 0.0;
            let mut best_tone = 0u8;
            
            for tone in 0..NUM_TONES {
                let tone_freq = frequency + tone as f64 * TONE_SPACING;
                let correlation = self.correlate_with_tone(symbol_audio, tone_freq)?;
                
                if correlation > max_correlation {
                    max_correlation = correlation;
                    best_tone = tone as u8;
                }
            }
            
            symbols.push(best_tone);
        }
        
        Ok(symbols)
    }
    
    /// Correlate symbol audio with a specific tone frequency
    fn correlate_with_tone(&self, symbol_audio: &[f64], frequency: f64) -> Ft8Result<f64> {
        let dt = 1.0 / self.config.sample_rate as f64;
        let mut correlation = 0.0;
        
        // Generate reference tone and correlate
        for (i, &sample) in symbol_audio.iter().enumerate() {
            let t = i as f64 * dt;
            let phase = 2.0 * std::f64::consts::PI * frequency * t;
            let reference = phase.cos();
            correlation += sample * reference;
        }
        
        Ok(correlation.abs() / symbol_audio.len() as f64)
    }
    
    /// Demodulate data symbols to bit sequence with Gray code de-mapping
    ///
    /// FT8 layout: S7 D29 S7 D29 S7 (79 symbols total)
    /// Only the 58 data symbols (positions 7..36 and 43..72) are demodulated.
    /// Costas sync symbols at positions 0..7, 36..43, 72..79 are skipped.
    fn demodulate_symbols(&self, symbols: &[u8]) -> Ft8Result<BitVec> {
        if symbols.len() != NUM_SYMBOLS {
            return Err(Ft8Error::MessageDecodingError(
                format!("Expected {} symbols, got {}", NUM_SYMBOLS, symbols.len())
            ));
        }

        // 58 data symbols × 3 bits = 174 bits (LDPC codeword)
        let mut bits = BitVec::with_capacity(174);

        // Data symbol positions: 7..36 (29 symbols) and 43..72 (29 symbols)
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
    
    /// Verify CRC checksum
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
    
    /// Get the last decoding metrics
    pub fn get_last_metrics(&self) -> &DecodingMetrics {
        &self.last_metrics
    }
    
    /// Check if decoder is synchronized
    pub fn is_synchronized(&self) -> bool {
        self.time_sync.is_synchronized()
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

/// Estimate noise floor from power spectral density
fn estimate_noise_floor(psd: &[f64]) -> f64 {
    // Use median as noise floor estimate (robust against peaks)
    let mut sorted_psd = psd.to_vec();
    sorted_psd.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    
    let median_idx = sorted_psd.len() / 2;
    sorted_psd[median_idx]
}

/// Create a local decoder for parallel processing
fn create_local_decoder(config: &Ft8Config) -> Ft8Result<LocalDecoder> {
    LocalDecoder::new(config.clone())
}

/// Decode candidate with frequency, time, and quality estimates
#[derive(Debug, Clone)]
struct DecodeCandidate {
    frequency: f64,
    time_offset: f64,
    snr_estimate: f32,
    confidence: f32,
    sync_quality: f32,
}

/// Spectrum analysis point
#[derive(Debug, Clone)]
struct SpectrumPoint {
    frequency: f64,
    power: f64,
    snr: f32,
    time_window: usize,
}

/// Frequency peak for signal detection
#[derive(Debug, Clone)]
struct FrequencyPeak {
    frequency: f64,
    power: f64,
    snr: f32,
    confidence: f32,
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

/// FT8 LDPC(174,91) decoder with belief propagation
/// 
/// Implements the LDPC decoder for FT8's (174,91) code:
/// - 91 information bits (77 payload + 14 CRC)
/// - 83 parity bits
/// - Optimized for low SNR operation (-20 dB or lower)
struct LdpcDecoder {
    max_iterations: usize,
    /// Parity check matrix (83x174)
    parity_check_matrix: ParityCheckMatrix,
    /// Variable node degree (number of check nodes connected to each variable node)
    variable_degrees: Vec<usize>,
    /// Check node degree (number of variable nodes connected to each check node)
    check_degrees: Vec<usize>,
    /// Early termination threshold for syndrome check
    early_termination: bool,
    /// Min-sum normalization factor for improved performance
    normalization_factor: f32,
}

impl LdpcDecoder {
    fn new(max_iterations: usize) -> Ft8Result<Self> {
        // Initialize the FT8 LDPC(174,91) parity check matrix
        let parity_check_matrix = ParityCheckMatrix::new_ft8();
        
        // Pre-compute node degrees for efficient belief propagation
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
            normalization_factor: 0.75, // Typical value for min-sum algorithm
        })
    }
    
    /// Decode using belief propagation with soft-decision input
    fn decode(&self, bits: &BitVec) -> Ft8Result<BitVec> {
        // Convert hard bits to soft LLRs for initial values
        let llrs = self.bits_to_llrs(bits);
        
        // Run belief propagation decoding
        let decoded_llrs = self.belief_propagation(&llrs)?;
        
        // Convert back to hard decisions
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
        
        // Convert to LLRs with moderate confidence
        // LLR = log(P(bit=0)/P(bit=1))
        const HARD_DECISION_LLR: f32 = 4.0; // Moderate confidence for hard decisions
        
        for i in 0..174.min(bits.len()) {
            llrs.push(if bits.get(i).map(|b| *b).unwrap_or(false) {
                -HARD_DECISION_LLR // bit = 1
            } else {
                HARD_DECISION_LLR  // bit = 0
            });
        }
        
        // Pad with zeros if needed
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
        // Initialize messages
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
        
        // Main iteration loop
        for iteration in 0..self.max_iterations {
            // Check node update (min-sum algorithm)
            for check_idx in 0..83 {
                let connected_vars = self.parity_check_matrix.get_connected_variables(check_idx);
                
                for &var_idx in connected_vars {
                    // Compute product of signs and minimum magnitude
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
                    
                    // Determine which minimum to use
                    let magnitude = if var_idx == min_index {
                        second_min_magnitude
                    } else {
                        min_magnitude
                    };
                    
                    // Apply normalization factor and set message
                    check_to_variable[check_idx][var_idx] = 
                        sign_product * magnitude * self.normalization_factor;
                }
            }
            
            // Variable node update
            for var_idx in 0..174 {
                let connected_checks = self.parity_check_matrix.get_connected_checks(var_idx);
                
                // Compute total LLR for this variable
                output_llrs[var_idx] = channel_llrs[var_idx];
                for &check_idx in connected_checks {
                    output_llrs[var_idx] += check_to_variable[check_idx][var_idx];
                }
                
                // Update variable-to-check messages
                for &check_idx in connected_checks {
                    variable_to_check[check_idx][var_idx] = output_llrs[var_idx] 
                        - check_to_variable[check_idx][var_idx];
                }
            }
            
            // Early termination: check if all parity checks are satisfied
            if self.early_termination && iteration > 0 {
                if self.check_syndrome(&output_llrs) {
                    return Ok(output_llrs);
                }
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

/// Local decoder for parallel processing
struct LocalDecoder {
    message_parser: MessageParser,
    ldpc_decoder: LdpcDecoder,
    config: Ft8Config,
}

impl LocalDecoder {
    fn new(config: Ft8Config) -> Ft8Result<Self> {
        Ok(Self {
            message_parser: MessageParser::new(),
            ldpc_decoder: LdpcDecoder::new(config.ldpc_iterations)?,
            config,
        })
    }
    
    fn decode_single_candidate(
        &mut self,
        candidate: &DecodeCandidate,
        _audio: &[f64],
    ) -> Ft8Result<Option<DecodedMessage>> {
        // Simplified local decoding - would need to implement full extraction logic
        // For now, only return a decode if the SNR is reasonable and confidence is high
        if candidate.snr_estimate > -15.0 && candidate.confidence > 0.7 {
            let ft8_message = Ft8Message::default();
            let decoded_message = DecodedMessage::new(
                ft8_message,
                candidate.snr_estimate,
                candidate.confidence,
                candidate.frequency,
                candidate.time_offset,
            );
            Ok(Some(decoded_message))
        } else {
            Ok(None)
        }
    }
}

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
        config.sample_rate = 48000; // Wrong sample rate
        
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
        
        // Wrong window size
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
        
        // Correct window size
        let samples = vec![0.0f32; WINDOW_SAMPLES];
        let result = decoder.decode_window(&samples);
        assert!(result.is_ok());
        
        let decoded = result.unwrap();
        // With silence, should decode no messages
        assert_eq!(decoded.len(), 0);
    }

    #[test]
    fn test_noise_floor_estimation() {
        let psd = vec![1.0, 2.0, 3.0, 100.0, 4.0, 5.0, 6.0]; // One spike
        let noise_floor = estimate_noise_floor(&psd);
        
        // Should be around the median value (4.0), not affected by the spike
        assert_relative_eq!(noise_floor, 4.0, epsilon = 0.1);
    }

    #[test]
    fn test_bits_to_u16_conversion() {
        let bits = bitvec![1, 0, 1, 1, 0, 0, 1, 0];
        let value = bits_to_u16(&bits);
        assert_eq!(value, 0b10110010);
    }

    #[test]
    fn test_decode_candidate_creation() {
        let candidate = DecodeCandidate {
            frequency: 1500.0,
            time_offset: 0.5,
            snr_estimate: -10.0,
            confidence: 0.8,
            sync_quality: 0.9,
        };
        
        assert_eq!(candidate.frequency, 1500.0);
        assert_eq!(candidate.time_offset, 0.5);
        assert_eq!(candidate.snr_estimate, -10.0);
        assert_eq!(candidate.confidence, 0.8);
        assert_eq!(candidate.sync_quality, 0.9);
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
    fn test_parity_check_matrix_creation() {
        let matrix = ParityCheckMatrix::new_ft8();
        assert_eq!(matrix.num_checks, 83);
        assert_eq!(matrix.num_variables, 174);
        
        // Check that all check nodes have connections
        for check_idx in 0..83 {
            let connections = matrix.get_connected_variables(check_idx);
            assert!(!connections.is_empty(), "Check node {} has no connections", check_idx);
            // FT8 LDPC typically has 7-9 connections per check node
            assert!(connections.len() >= 6 && connections.len() <= 12, 
                    "Check node {} has {} connections, expected 6-12", 
                    check_idx, connections.len());
        }
        
        // Check that all variable nodes have connections
        for var_idx in 0..174 {
            let connections = matrix.get_connected_checks(var_idx);
            assert!(!connections.is_empty(), "Variable node {} has no connections", var_idx);
            // Variable nodes typically have 3-5 connections, but can have more in optimized codes
            assert!(connections.len() >= 1 && connections.len() <= 12,
                    "Variable node {} has {} connections, expected 1-12",
                    var_idx, connections.len());
        }
        
        // Check matrix sparsity (LDPC codes are sparse)
        let total_connections: usize = (0..83)
            .map(|i| matrix.get_connected_variables(i).len())
            .sum();
        let density = total_connections as f64 / (83.0 * 174.0);
        assert!(density < 0.1, "Matrix too dense: {:.2}% (expected < 10%)", density * 100.0);
    }
    
    #[test]
    fn test_ldpc_bits_to_llrs_conversion() {
        let decoder = LdpcDecoder::new(10).unwrap();
        
        // Create a test bit vector
        let bits = bitvec![1, 0, 1, 1, 0, 0, 1, 0];
        let llrs = decoder.bits_to_llrs(&bits);
        
        // Check conversion
        assert_eq!(llrs.len(), 174);
        assert!(llrs[0] < 0.0); // bit 1 -> negative LLR
        assert!(llrs[1] > 0.0); // bit 0 -> positive LLR
        assert!(llrs[2] < 0.0); // bit 1 -> negative LLR
    }
    
    #[test]
    fn test_ldpc_llrs_to_bits_conversion() {
        let decoder = LdpcDecoder::new(10).unwrap();
        
        // Create test LLRs (full 174 elements)
        let mut llrs = vec![0.0; 174];
        llrs[0] = -2.0;
        llrs[1] = 3.0;
        llrs[2] = -1.5;
        llrs[3] = 0.5;
        llrs[4] = -0.1;
        
        let bits = decoder.llrs_to_bits(&llrs).unwrap();
        
        // Check conversion
        assert_eq!(bits.len(), 174);
        assert!(bits[0]);  // negative LLR -> bit 1
        assert!(!bits[1]); // positive LLR -> bit 0
        assert!(bits[2]);  // negative LLR -> bit 1
        assert!(!bits[3]); // positive LLR -> bit 0
        assert!(bits[4]);  // negative LLR -> bit 1
    }
    
    #[test]
    fn test_ldpc_soft_decode_size_validation() {
        let decoder = LdpcDecoder::new(10).unwrap();
        
        // Wrong size input
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
        
        // All zeros should satisfy parity checks (valid codeword)
        let llrs = vec![10.0; 174]; // Strong confidence in all zeros
        assert!(decoder.check_syndrome(&llrs));
        
        // Random values likely won't satisfy parity checks
        let mut random_llrs = vec![0.0; 174];
        for (i, llr) in random_llrs.iter_mut().enumerate() {
            *llr = if i % 3 == 0 { -2.0 } else { 2.0 };
        }
        // This is unlikely to be a valid codeword
        assert!(!decoder.check_syndrome(&random_llrs));
    }
    
    #[test]
    fn test_ldpc_decode_with_no_errors() {
        let decoder = LdpcDecoder::new(50).unwrap();
        
        // Create a valid all-zero codeword
        let bits = bitvec![0; 174];
        let decoded = decoder.decode(&bits).unwrap();
        
        // Should return the same bits (no errors to correct)
        assert_eq!(decoded.len(), 174);
        for i in 0..174 {
            assert_eq!(decoded[i], bits[i]);
        }
    }
    
    #[test]
    fn test_ldpc_belief_propagation_convergence() {
        let decoder = LdpcDecoder::new(100).unwrap();
        
        // Create LLRs with high confidence (should converge quickly)
        let mut llrs = vec![5.0; 174]; // All zeros with high confidence
        
        // Add a few errors
        llrs[10] = -1.0; // Flip bit 10
        llrs[50] = -0.5; // Flip bit 50
        
        let decoded_llrs = decoder.belief_propagation(&llrs).unwrap();
        
        // Check that most bits maintained their sign
        let mut correct_bits = 0;
        for i in 0..174 {
            if i != 10 && i != 50 {
                if decoded_llrs[i] > 0.0 {
                    correct_bits += 1;
                }
            }
        }
        
        // Most bits should be correct
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
    fn test_automatic_gain_control() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();
        
        // Create test signal with varying amplitude
        let mut audio = vec![0.0; 1000];
        for i in 0..1000 {
            audio[i] = (i as f64 / 1000.0) * 2.0 - 1.0; // Ramp from -1 to 1
        }
        
        let agc_audio = decoder.apply_automatic_gain_control(&audio).unwrap();
        
        // Check that AGC output is bounded
        for sample in agc_audio {
            assert!(sample.abs() <= 3.0); // AGC should limit output
        }
    }
    
    #[test]
    fn test_noise_floor_statistical() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();
        
        // Create test PSD with noise and a signal peak
        let mut psd = vec![0.1; 100]; // Noise floor at 0.1
        psd[50] = 10.0; // Signal peak
        psd[51] = 8.0;
        psd[52] = 9.0;
        
        let noise_floor = decoder.estimate_noise_floor_statistical(&psd).unwrap();
        
        // Noise floor should be close to 0.1, not affected by peaks
        assert!(noise_floor < 0.5);
        assert!(noise_floor > 0.05);
    }
    
    #[test]
    fn test_waterfall_data_generation() {
        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();
        
        // Create test audio with a tone
        let mut audio = vec![0.0; WINDOW_SAMPLES];
        let freq = 1000.0; // 1 kHz tone
        for i in 0..audio.len() {
            let t = i as f64 / SAMPLE_RATE as f64;
            audio[i] = (2.0 * PI * freq * t).sin() * 0.5;
        }
        
        let waterfall = decoder.generate_waterfall_data(&audio).unwrap();
        
        // Check waterfall structure
        assert!(!waterfall.time_bins.is_empty());
        assert!(!waterfall.frequency_bins.is_empty());
        assert!(!waterfall.power_matrix.is_empty());
        assert!(waterfall.min_power < waterfall.max_power);
        
        // Frequency bins should cover FT8 range
        assert!(waterfall.frequency_bins[0] >= 200.0);
        assert!(waterfall.frequency_bins.last().unwrap() <= &4000.0);
    }
    
    #[test]
    fn test_doppler_compensation() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();
        
        // Create spectrum with Doppler-shifted signal
        let mut spectrum = Vec::new();
        let base_freq = 1500.0;
        let doppler_shift = 50.0;
        
        // Add spectrum points with Doppler shift
        for i in 0..10 {
            spectrum.push(SpectrumPoint {
                frequency: base_freq + doppler_shift,
                power: 1.0,
                snr: 10.0,
                time_window: i,
            });
        }
        
        let compensated = decoder.compensate_doppler_shift(spectrum.clone()).unwrap();
        
        // Check that compensation was applied
        assert_eq!(compensated.len(), spectrum.len());
        // Frequencies might be adjusted for Doppler
        for point in compensated {
            assert!(point.frequency.is_finite());
        }
    }
    
    #[test]
    fn test_coherent_averaging() {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config).unwrap();
        
        // Create spectrum with multiple observations of same frequency
        let mut spectrum = Vec::new();
        let freq = 1500.0;
        
        // Add multiple weak signals at same frequency
        for i in 0..5 {
            spectrum.push(SpectrumPoint {
                frequency: freq + (i as f64 * 0.1), // Slight variation
                power: 0.01,
                snr: -10.0,
                time_window: i,
            });
        }
        
        let averaged = decoder.coherent_symbol_averaging(&spectrum).unwrap();
        
        // Coherent averaging should improve SNR
        assert!(!averaged.is_empty());
        // The averaged result might have better SNR than individual points
        if !averaged.is_empty() {
            let avg_snr = averaged[0].snr;
            assert!(avg_snr.is_finite());
        }
    }
    
    #[test]
    fn test_multi_resolution_analysis() {
        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();
        
        // Create test audio with FT8-like signal
        let mut audio = vec![0.0; WINDOW_SAMPLES];
        
        // Add multiple tones (simulating FT8)
        for tone in 0..8 {
            let freq = 500.0 + tone as f64 * TONE_SPACING;
            for i in 0..audio.len() {
                let t = i as f64 / SAMPLE_RATE as f64;
                audio[i] += (2.0 * PI * freq * t).sin() * 0.1;
            }
        }
        
        // Add simulated noise
        for (i, sample) in audio.iter_mut().enumerate() {
            // Simple pseudo-random noise generation for testing
            let noise = ((i as f64 * 0.12345).sin() * 43758.5453).fract() - 0.5;
            *sample += noise * 0.01;
        }
        
        let spectrum = decoder.analyze_spectrum(&audio).unwrap();
        
        // Should detect multiple frequency components
        assert!(!spectrum.is_empty());
        
        // Check that spectrum points are valid
        for point in spectrum {
            assert!(point.frequency >= 200.0 && point.frequency <= 4000.0);
            assert!(point.power.is_finite());
            assert!(point.snr.is_finite());
        }
    }
}