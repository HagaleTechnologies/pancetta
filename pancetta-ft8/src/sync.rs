//! Time synchronization for FT8 decoding
//!
//! FT8 requires precise time synchronization to decode messages correctly:
//! - 15-second transmission windows
//! - ±1 second tolerance for decoding
//! - Symbol timing recovery
//! - Frame boundary detection
//! - Automatic time correction

use crate::{
    signal_processing::{FftProcessor, SymbolCorrelator, WindowFunction},
    Ft8Result, SAMPLE_RATE, SYMBOL_DURATION,
};
use std::collections::VecDeque;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Time synchronization tolerance in seconds
pub const SYNC_TOLERANCE: f64 = 1.0;

/// Minimum correlation threshold for sync detection
pub const MIN_SYNC_CORRELATION: f64 = 0.3;

/// Number of historical sync measurements to keep
const SYNC_HISTORY_SIZE: usize = 10;

/// Time synchronization results
#[derive(Debug, Clone)]
pub struct SyncResult {
    /// Whether synchronization was successful
    pub synchronized: bool,
    /// Time offset from expected sync point (seconds)
    pub time_offset: f64,
    /// Synchronization confidence (0.0 - 1.0)
    pub confidence: f64,
    /// Symbol timing accuracy (seconds)
    pub symbol_timing: f64,
    /// Frame boundary position (samples)
    pub frame_boundary: usize,
    /// Detected transmission start time
    pub transmission_start: SystemTime,
    /// Quality metrics
    pub quality_metrics: SyncQualityMetrics,
}

impl Default for SyncResult {
    fn default() -> Self {
        Self {
            synchronized: false,
            time_offset: 0.0,
            confidence: 0.0,
            symbol_timing: 0.0,
            frame_boundary: 0,
            transmission_start: SystemTime::now(),
            quality_metrics: SyncQualityMetrics::default(),
        }
    }
}

/// Quality metrics for synchronization assessment
#[derive(Debug, Clone, Default)]
pub struct SyncQualityMetrics {
    /// Peak correlation value
    pub peak_correlation: f64,
    /// Signal-to-noise ratio of sync signal
    pub sync_snr: f64,
    /// Stability of sync over time
    pub stability: f64,
    /// Number of detected sync patterns
    pub sync_patterns: usize,
    /// Timing jitter (RMS variation)
    pub timing_jitter: f64,
}

/// Time synchronization engine for FT8
pub struct TimeSync {
    /// Sample rate for processing
    sample_rate: u32,
    /// Symbol correlator for timing detection
    symbol_correlator: SymbolCorrelator,
    /// FFT processor for spectral analysis
    fft_processor: FftProcessor,
    /// Historical sync measurements
    sync_history: VecDeque<SyncMeasurement>,
    /// Current sync state
    current_sync: Option<SyncResult>,
    /// Time reference for synchronization
    time_reference: SystemTime,
    /// Samples per symbol
    samples_per_symbol: usize,
    /// Expected UTC sync times (every 15 seconds)
    next_sync_time: SystemTime,
}

/// Internal sync measurement
#[derive(Debug, Clone)]
struct SyncMeasurement {
    timestamp: SystemTime,
    offset: f64,
    correlation: f64,
    confidence: f64,
}

impl TimeSync {
    /// Create a new time synchronization engine
    pub fn new() -> Ft8Result<Self> {
        let sample_rate = SAMPLE_RATE;
        let symbol_correlator = SymbolCorrelator::new()?;
        let fft_processor = FftProcessor::new(4096, WindowFunction::Hann)?;
        let samples_per_symbol = (SYMBOL_DURATION * sample_rate as f64) as usize;

        // Calculate next 15-second boundary
        let now = SystemTime::now();
        let next_sync_time = calculate_next_sync_time(now);

        Ok(Self {
            sample_rate,
            symbol_correlator,
            fft_processor,
            sync_history: VecDeque::with_capacity(SYNC_HISTORY_SIZE),
            current_sync: None,
            time_reference: now,
            samples_per_symbol,
            next_sync_time,
        })
    }

    /// Update time reference (call periodically to maintain accuracy)
    pub fn update_time_reference(&mut self) {
        self.time_reference = SystemTime::now();
        self.next_sync_time = calculate_next_sync_time(self.time_reference);
    }

    /// Synchronize with incoming audio data
    pub fn synchronize(
        &mut self,
        audio_data: &[f32],
        capture_time: SystemTime,
    ) -> Ft8Result<SyncResult> {
        // Convert to f64 for processing
        let audio_f64: Vec<f64> = audio_data.iter().map(|&x| x as f64).collect();

        // Detect FT8 sync patterns
        let sync_patterns = self.detect_sync_patterns(&audio_f64)?;

        // Find best sync candidate
        let best_sync = self.find_best_sync_candidate(&sync_patterns, capture_time)?;

        // Validate sync timing against UTC
        let validated_sync = self.validate_utc_timing(best_sync, capture_time)?;

        // Update sync history
        self.update_sync_history(&validated_sync);

        // Calculate quality metrics
        let mut result = validated_sync;
        result.quality_metrics = self.calculate_quality_metrics()?;

        self.current_sync = Some(result.clone());
        Ok(result)
    }

    /// Detect sync patterns in audio data
    fn detect_sync_patterns(&mut self, audio_data: &[f64]) -> Ft8Result<Vec<SyncCandidate>> {
        let mut candidates = Vec::new();

        // Use symbol correlator to find sync patterns
        let correlations = self.symbol_correlator.correlate(audio_data)?;

        // Find correlation peaks
        let peaks = find_correlation_peaks(&correlations, MIN_SYNC_CORRELATION);

        for peak in peaks {
            // Calculate timing metrics for this candidate
            let timing_offset = peak.position as f64 / self.sample_rate as f64;
            let symbol_alignment = timing_offset % SYMBOL_DURATION;

            // Check if this looks like a valid FT8 sync pattern
            if self.is_valid_sync_pattern(audio_data, peak.position)? {
                candidates.push(SyncCandidate {
                    sample_position: peak.position,
                    time_offset: timing_offset,
                    correlation: peak.value,
                    symbol_alignment,
                    confidence: self.calculate_sync_confidence(peak.value, symbol_alignment),
                });
            }
        }

        // Sort by confidence
        candidates.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(candidates)
    }

    /// Check if detected pattern looks like valid FT8 sync
    fn is_valid_sync_pattern(&mut self, audio_data: &[f64], position: usize) -> Ft8Result<bool> {
        // Check if we have enough data for analysis
        let analysis_window = self.samples_per_symbol * 8; // Analyze 8 symbols
        if position + analysis_window > audio_data.len() {
            return Ok(false);
        }

        // Extract window around sync position
        let window = &audio_data[position..position + analysis_window];

        // Perform spectral analysis to look for FT8 characteristics
        let spectrum = self.fft_processor.power_spectral_density(window)?;
        let freq_bins = self.fft_processor.frequency_bins();

        // Look for FT8 tone spacing pattern
        let tone_energy = self.measure_tone_energy(&spectrum, &freq_bins);

        // Check for expected FT8 spectral characteristics
        Ok(tone_energy > 0.1) // Threshold for tone presence
    }

    /// Measure energy in FT8 tone frequencies
    fn measure_tone_energy(&self, spectrum: &[f64], freq_bins: &[f64]) -> f64 {
        let base_freq = 1500.0; // Typical FT8 base frequency
        let tone_spacing = 6.25; // Hz
        let mut total_energy = 0.0;

        for tone in 0..8 {
            let freq = base_freq + tone as f64 * tone_spacing;

            // Find closest frequency bin
            if let Some(bin_idx) = freq_bins.iter().position(|&f| f >= freq) {
                if bin_idx < spectrum.len() {
                    total_energy += spectrum[bin_idx];
                }
            }
        }

        total_energy
    }

    /// Find the best sync candidate
    fn find_best_sync_candidate(
        &self,
        candidates: &[SyncCandidate],
        _capture_time: SystemTime,
    ) -> Ft8Result<SyncResult> {
        if candidates.is_empty() {
            return Ok(SyncResult::default());
        }

        // Use the highest confidence candidate
        let best = &candidates[0];

        Ok(SyncResult {
            synchronized: best.confidence > MIN_SYNC_CORRELATION,
            time_offset: best.time_offset,
            confidence: best.confidence,
            symbol_timing: best.symbol_alignment,
            frame_boundary: best.sample_position,
            transmission_start: SystemTime::now() - Duration::from_secs_f64(best.time_offset),
            quality_metrics: SyncQualityMetrics::default(),
        })
    }

    /// Validate sync timing against UTC 15-second boundaries
    fn validate_utc_timing(
        &self,
        sync_result: SyncResult,
        capture_time: SystemTime,
    ) -> Ft8Result<SyncResult> {
        if !sync_result.synchronized {
            return Ok(sync_result);
        }

        // Calculate expected UTC sync time
        let expected_sync = calculate_expected_sync_time(capture_time);
        let actual_sync = sync_result.transmission_start;

        // Calculate timing error
        let timing_error = actual_sync
            .duration_since(expected_sync)
            .unwrap_or_else(|_| expected_sync.duration_since(actual_sync).unwrap())
            .as_secs_f64();

        // Check if within tolerance
        let mut validated = sync_result;
        if timing_error <= SYNC_TOLERANCE {
            validated.synchronized = true;
            validated.confidence *= 1.0 - (timing_error / SYNC_TOLERANCE) * 0.2;
        // Reduce confidence based on error
        } else {
            validated.synchronized = false;
            validated.confidence *= 0.5; // Significantly reduce confidence
        }

        Ok(validated)
    }

    /// Update synchronization history
    fn update_sync_history(&mut self, sync_result: &SyncResult) {
        if sync_result.synchronized {
            let measurement = SyncMeasurement {
                timestamp: SystemTime::now(),
                offset: sync_result.time_offset,
                correlation: sync_result.quality_metrics.peak_correlation,
                confidence: sync_result.confidence,
            };

            self.sync_history.push_back(measurement);

            // Keep only recent measurements
            while self.sync_history.len() > SYNC_HISTORY_SIZE {
                self.sync_history.pop_front();
            }
        }
    }

    /// Calculate sync confidence based on correlation and symbol alignment
    fn calculate_sync_confidence(&self, correlation: f64, symbol_alignment: f64) -> f64 {
        // Base confidence from correlation strength
        let correlation_confidence =
            (correlation - MIN_SYNC_CORRELATION) / (1.0 - MIN_SYNC_CORRELATION);

        // Penalty for poor symbol alignment
        let alignment_penalty = (symbol_alignment / SYMBOL_DURATION).min(1.0);
        let alignment_confidence = 1.0 - alignment_penalty;

        // Combined confidence
        (correlation_confidence * 0.7 + alignment_confidence * 0.3).clamp(0.0, 1.0)
    }

    /// Calculate quality metrics for current sync state
    fn calculate_quality_metrics(&self) -> Ft8Result<SyncQualityMetrics> {
        let mut metrics = SyncQualityMetrics::default();

        if self.sync_history.is_empty() {
            return Ok(metrics);
        }

        // Calculate peak correlation
        metrics.peak_correlation = self
            .sync_history
            .iter()
            .map(|m| m.correlation)
            .fold(0.0f64, f64::max);

        // Calculate stability (inverse of timing variance)
        let mean_offset: f64 = self.sync_history.iter().map(|m| m.offset).sum::<f64>()
            / self.sync_history.len() as f64;
        let variance: f64 = self
            .sync_history
            .iter()
            .map(|m| (m.offset - mean_offset).powi(2))
            .sum::<f64>()
            / self.sync_history.len() as f64;

        metrics.timing_jitter = variance.sqrt();
        metrics.stability = if variance > 0.0 {
            1.0 / (1.0 + variance)
        } else {
            1.0
        };

        // Count detected patterns
        metrics.sync_patterns = self.sync_history.len();

        // Estimate SNR (simplified)
        metrics.sync_snr = 20.0 * metrics.peak_correlation.log10();

        Ok(metrics)
    }

    /// Get current synchronization status
    pub fn get_sync_status(&self) -> Option<&SyncResult> {
        self.current_sync.as_ref()
    }

    /// Check if currently synchronized
    pub fn is_synchronized(&self) -> bool {
        self.current_sync.as_ref().map_or(false, |s| s.synchronized)
    }

    /// Get time until next expected sync window
    pub fn time_to_next_sync(&self) -> Duration {
        self.next_sync_time
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO)
    }
}

impl Default for TimeSync {
    fn default() -> Self {
        Self::new().expect("Failed to create default TimeSync")
    }
}

/// Sync candidate detected in audio
#[derive(Debug, Clone)]
struct SyncCandidate {
    sample_position: usize,
    time_offset: f64,
    correlation: f64,
    symbol_alignment: f64,
    confidence: f64,
}

/// Correlation peak in signal
#[derive(Debug, Clone)]
struct CorrelationPeak {
    position: usize,
    value: f64,
}

/// Find peaks in correlation signal
fn find_correlation_peaks(correlations: &[f64], threshold: f64) -> Vec<CorrelationPeak> {
    let mut peaks = Vec::new();

    for i in 1..correlations.len() - 1 {
        if correlations[i] > threshold
            && correlations[i] > correlations[i - 1]
            && correlations[i] > correlations[i + 1]
        {
            peaks.push(CorrelationPeak {
                position: i,
                value: correlations[i],
            });
        }
    }

    // Sort by correlation value (descending)
    peaks.sort_by(|a, b| {
        b.value
            .partial_cmp(&a.value)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    peaks
}

/// Calculate next UTC sync time (15-second boundary)
fn calculate_next_sync_time(current_time: SystemTime) -> SystemTime {
    let since_epoch = current_time
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);

    let seconds_since_epoch = since_epoch.as_secs();
    let next_15_second_boundary = ((seconds_since_epoch / 15) + 1) * 15;

    UNIX_EPOCH + Duration::from_secs(next_15_second_boundary)
}

/// Calculate expected sync time for given capture time
fn calculate_expected_sync_time(capture_time: SystemTime) -> SystemTime {
    let since_epoch = capture_time
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);

    let seconds_since_epoch = since_epoch.as_secs();
    let last_15_second_boundary = (seconds_since_epoch / 15) * 15;

    UNIX_EPOCH + Duration::from_secs(last_15_second_boundary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn test_time_sync_creation() {
        let sync = TimeSync::new();
        assert!(sync.is_ok());
    }

    #[test]
    fn test_sync_result_default() {
        let result = SyncResult::default();
        assert!(!result.synchronized);
        assert_eq!(result.confidence, 0.0);
        assert_eq!(result.time_offset, 0.0);
    }

    #[test]
    fn test_correlation_peak_finding() {
        let correlations = vec![0.1, 0.5, 0.3, 0.8, 0.2, 0.6, 0.1];
        let peaks = find_correlation_peaks(&correlations, 0.4);

        // Should find peaks at positions 1, 3, and 5
        assert_eq!(peaks.len(), 3);
        assert_eq!(peaks[0].position, 3); // Highest peak first
        assert_relative_eq!(peaks[0].value, 0.8, epsilon = 1e-10);
    }

    #[test]
    fn test_sync_time_calculations() {
        // Test with a known time
        let test_time = UNIX_EPOCH + Duration::from_secs(1634567890); // Some arbitrary time

        let next_sync = calculate_next_sync_time(test_time);
        let expected_sync = calculate_expected_sync_time(test_time);

        // Next sync should be after current time
        assert!(next_sync > test_time);

        // Expected sync should be before or at current time
        assert!(expected_sync <= test_time);

        // Both should be on 15-second boundaries
        let next_seconds = next_sync.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let expected_seconds = expected_sync.duration_since(UNIX_EPOCH).unwrap().as_secs();

        assert_eq!(next_seconds % 15, 0);
        assert_eq!(expected_seconds % 15, 0);
    }

    #[test]
    fn test_sync_confidence_calculation() {
        let sync = TimeSync::new().unwrap();

        // High correlation, good alignment
        let confidence1 = sync.calculate_sync_confidence(0.8, 0.01);
        assert!(confidence1 > 0.7);

        // Low correlation, poor alignment
        let confidence2 = sync.calculate_sync_confidence(0.4, 0.15);
        assert!(confidence2 < 0.5);

        // Medium correlation, medium alignment
        let confidence3 = sync.calculate_sync_confidence(0.6, 0.08);
        assert!(confidence3 > confidence2 && confidence3 < confidence1);
    }

    #[test]
    fn test_quality_metrics_default() {
        let metrics = SyncQualityMetrics::default();
        assert_eq!(metrics.peak_correlation, 0.0);
        assert_eq!(metrics.sync_snr, 0.0);
        assert_eq!(metrics.stability, 0.0);
        assert_eq!(metrics.sync_patterns, 0);
        assert_eq!(metrics.timing_jitter, 0.0);
    }

    #[test]
    fn test_sync_history_management() {
        let mut sync = TimeSync::new().unwrap();

        // Initially no sync
        assert!(!sync.is_synchronized());
        assert!(sync.get_sync_status().is_none());

        // Update time reference
        sync.update_time_reference();
        assert!(sync.time_to_next_sync().as_secs() <= 15);
    }
}
