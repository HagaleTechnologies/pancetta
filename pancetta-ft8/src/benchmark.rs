//! Benchmark harness for comparing Pancetta's FT8 decoder against ft8_lib.
//!
//! This module provides structured decode output and a comparison framework
//! so decoder improvements can be measured against the reference implementation.

use std::collections::HashSet;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::ft8_lib_ffi::ft8lib_decode_audio;
use crate::{Ft8Config, Ft8Decoder, WINDOW_SAMPLES};

// ============================================================================
// Data structures
// ============================================================================

/// A single decoded FT8 message with its RF metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodeResult {
    /// The decoded message text (e.g. "CQ W1ABC FN42").
    pub message: String,

    /// Centre frequency of the signal in Hz.
    pub frequency_hz: f64,

    /// Time offset relative to the start of the FT8 window, in seconds.
    pub time_offset_s: f64,

    /// Signal-to-noise ratio in dB.
    pub snr_db: f32,
}

/// Decode results for a single WAV file from both decoders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResult {
    /// Path to the WAV file that was decoded.
    pub file_path: String,

    /// Messages decoded by Pancetta.
    pub pancetta_decodes: Vec<DecodeResult>,

    /// Messages decoded by ft8_lib.
    pub ft8lib_decodes: Vec<DecodeResult>,

    /// Total wall-clock time taken for both decode passes, in milliseconds.
    pub processing_time_ms: f64,
}

/// Aggregated comparison across multiple WAV files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonSummary {
    /// Number of WAV files compared.
    pub total_files: usize,

    /// Total messages decoded by Pancetta across all files.
    pub pancetta_total: usize,

    /// Total messages decoded by ft8_lib across all files.
    pub ft8lib_total: usize,

    /// Messages decoded by *both* decoders (by message text).
    pub both_decoded: usize,

    /// Messages decoded only by Pancetta.
    pub pancetta_only: usize,

    /// Messages decoded only by ft8_lib.
    pub ft8lib_only: usize,

    /// Parity percentage: `both_decoded / max(pancetta_total, ft8lib_total) * 100`.
    /// A value of 100 means the decoders agree perfectly.
    pub parity_percent: f64,

    /// Per-file breakdown.
    pub per_file: Vec<BenchmarkResult>,
}

// ============================================================================
// WAV reading
// ============================================================================

/// Read a mono 12 kHz WAV file and return its samples as `f32`.
///
/// # Errors
/// Returns a descriptive error string if the file cannot be opened, is not
/// mono, has an unsupported sample format, or has the wrong sample rate.
pub fn read_wav_samples(path: &str) -> Result<Vec<f32>, String> {
    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("Cannot open WAV '{}': {}", path, e))?;

    let spec = reader.spec();

    if spec.channels != 1 {
        return Err(format!(
            "WAV '{}' has {} channels; expected mono (1)",
            path, spec.channels
        ));
    }

    if spec.sample_rate != 12_000 {
        return Err(format!(
            "WAV '{}' has sample rate {}; expected 12000 Hz",
            path, spec.sample_rate
        ));
    }

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.map_err(|e| format!("Read error in '{}': {}", path, e)))
            .collect::<Result<Vec<_>, _>>()?,

        hound::SampleFormat::Int => {
            // Read as i16 and normalize to [-1.0, 1.0].
            // Note: hound's samples::<i32>() left-shifts values to fill the i32
            // range, which would give incorrect normalization. Using i16 matches
            // the approach in wav_decode_tests.rs.
            reader
                .samples::<i16>()
                .map(|s| {
                    s.map(|v| v as f32 / 32768.0)
                        .map_err(|e| format!("Read error in '{}': {}", path, e))
                })
                .collect::<Result<Vec<_>, _>>()?
        }
    };

    Ok(samples)
}

// ============================================================================
// Decoding
// ============================================================================

/// Decode a WAV file with both Pancetta and ft8_lib, returning structured results.
///
/// The WAV file must be mono 12 kHz. If it contains more samples than one FT8
/// window (`WINDOW_SAMPLES`), only the first window is decoded.
///
/// # Errors
/// Returns a descriptive error string on WAV read failure or decoder
/// initialisation failure.
pub fn decode_wav_to_results(path: &str) -> Result<BenchmarkResult, String> {
    let samples = read_wav_samples(path)?;

    // Pad to WINDOW_SAMPLES if needed, pass all samples if longer.
    // The decoder needs at least WINDOW_SAMPLES but can handle more.
    let buffer: Vec<f32> = if samples.len() >= WINDOW_SAMPLES {
        samples.clone()
    } else {
        let mut padded = samples.clone();
        padded.resize(WINDOW_SAMPLES, 0.0);
        padded
    };

    let start = Instant::now();

    // --- Pancetta decode ---
    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config)
        .map_err(|e| format!("Ft8Decoder::new failed: {}", e))?;

    let pancetta_raw = decoder
        .decode_window(&buffer)
        .map_err(|e| format!("decode_window failed: {}", e))?;

    let pancetta_decodes: Vec<DecodeResult> = pancetta_raw
        .into_iter()
        .map(|m| DecodeResult {
            message: m.text,
            frequency_hz: m.frequency_offset,
            time_offset_s: m.time_offset,
            snr_db: m.snr_db,
        })
        .collect();

    // --- ft8_lib decode ---
    let ft8lib_raw = ft8lib_decode_audio(&buffer);

    let ft8lib_decodes: Vec<DecodeResult> = ft8lib_raw
        .into_iter()
        .map(|(msg, freq, time, _ldpc_errors)| DecodeResult {
            message: msg,
            frequency_hz: freq as f64,
            time_offset_s: time as f64,
            snr_db: 0.0, // ft8_lib FFI does not return SNR
        })
        .collect();

    let processing_time_ms = start.elapsed().as_secs_f64() * 1000.0;

    Ok(BenchmarkResult {
        file_path: path.to_string(),
        pancetta_decodes,
        ft8lib_decodes,
        processing_time_ms,
    })
}

// ============================================================================
// Comparison
// ============================================================================

/// Compare decode results across multiple WAV files and produce a summary.
///
/// Agreement is determined solely by message text (case-sensitive).
pub fn compare_results(results: &[BenchmarkResult]) -> ComparisonSummary {
    let total_files = results.len();

    let mut pancetta_total = 0usize;
    let mut ft8lib_total = 0usize;
    let mut both_decoded = 0usize;
    let mut pancetta_only = 0usize;
    let mut ft8lib_only = 0usize;

    for r in results {
        let p_set: HashSet<&str> = r.pancetta_decodes.iter().map(|d| d.message.as_str()).collect();
        let f_set: HashSet<&str> = r.ft8lib_decodes.iter().map(|d| d.message.as_str()).collect();

        let intersection = p_set.intersection(&f_set).count();
        let p_only = p_set.difference(&f_set).count();
        let f_only = f_set.difference(&p_set).count();

        pancetta_total += p_set.len();
        ft8lib_total += f_set.len();
        both_decoded += intersection;
        pancetta_only += p_only;
        ft8lib_only += f_only;
    }

    let denom = pancetta_total.max(ft8lib_total);
    let parity_percent = if denom == 0 {
        100.0
    } else {
        both_decoded as f64 / denom as f64 * 100.0
    };

    ComparisonSummary {
        total_files,
        pancetta_total,
        ft8lib_total,
        both_decoded,
        pancetta_only,
        ft8lib_only,
        parity_percent,
        per_file: results.to_vec(),
    }
}

// ============================================================================
// Unit tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compare_results_empty() {
        let summary = compare_results(&[]);
        assert_eq!(summary.total_files, 0);
        assert_eq!(summary.pancetta_total, 0);
        assert_eq!(summary.ft8lib_total, 0);
        assert_eq!(summary.both_decoded, 0);
        assert!((summary.parity_percent - 100.0).abs() < 1e-6);
    }

    #[test]
    fn test_compare_results_perfect_agreement() {
        let r = BenchmarkResult {
            file_path: "x.wav".to_string(),
            pancetta_decodes: vec![DecodeResult {
                message: "CQ W1ABC FN42".to_string(),
                frequency_hz: 1500.0,
                time_offset_s: 0.0,
                snr_db: -10.0,
            }],
            ft8lib_decodes: vec![DecodeResult {
                message: "CQ W1ABC FN42".to_string(),
                frequency_hz: 1500.0,
                time_offset_s: 0.0,
                snr_db: 0.0,
            }],
            processing_time_ms: 50.0,
        };

        let summary = compare_results(&[r]);
        assert_eq!(summary.both_decoded, 1);
        assert_eq!(summary.pancetta_only, 0);
        assert_eq!(summary.ft8lib_only, 0);
        assert!((summary.parity_percent - 100.0).abs() < 1e-6);
    }
}
