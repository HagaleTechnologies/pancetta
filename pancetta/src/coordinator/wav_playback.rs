use anyhow::Result;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use std::path::PathBuf;
use tracing::{debug, info};

use super::util::resample_linear;

/// Compute the integer-PCM normalization scale (`2^(bits-1)`) for a WAV's
/// `bits_per_sample`, validating it first.
///
/// `bits_per_sample` comes straight out of an attacker-controlled WAV header.
/// Rejecting out-of-range values up front avoids the `bits - 1` u16 underflow
/// (panic in debug / wrap in release) and the subsequent oversized
/// `1i64 << (bits - 1)` shift that the naive expression would hit on a
/// malformed `0` or absurdly large field. Valid PCM widths (8/16/24/32) are
/// unaffected.
fn int_sample_max_val(bits_per_sample: u16) -> Result<f32> {
    if !(1..=32).contains(&bits_per_sample) {
        anyhow::bail!(
            "Unsupported WAV bits_per_sample: {} (expected 1..=32)",
            bits_per_sample
        );
    }
    Ok((1i64 << (bits_per_sample - 1)) as f32)
}

impl super::ApplicationCoordinator {
    /// Run WAV playback mode: read file, decode, print results, exit.
    pub(crate) async fn run_wav_playback(&self, wav_path: PathBuf) -> Result<()> {
        info!("WAV playback mode: {}", wav_path.display());

        // Read WAV file
        let reader = hound::WavReader::open(&wav_path).map_err(|e| {
            anyhow::anyhow!("Failed to open WAV file {}: {}", wav_path.display(), e)
        })?;

        let spec = reader.spec();
        info!(
            "WAV: {} channels, {} Hz, {:?}, {} bits",
            spec.channels, spec.sample_rate, spec.sample_format, spec.bits_per_sample
        );

        // Read all samples as f32
        let raw_samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Int => {
                let max_val = int_sample_max_val(spec.bits_per_sample)?;
                reader
                    .into_samples::<i32>()
                    .filter_map(|s| s.ok())
                    .map(|s| s as f32 / max_val)
                    .collect()
            }
            hound::SampleFormat::Float => reader
                .into_samples::<f32>()
                .filter_map(|s| s.ok())
                .collect(),
        };

        info!("Read {} raw samples", raw_samples.len());

        // Mix down to mono if stereo
        let mono_samples: Vec<f32> = if spec.channels > 1 {
            let ch = spec.channels as usize;
            raw_samples
                .chunks(ch)
                .map(|frame| frame.iter().sum::<f32>() / ch as f32)
                .collect()
        } else {
            raw_samples
        };

        // Resample to 12 kHz if needed
        let target_rate = pancetta_ft8::SAMPLE_RATE;
        let samples_12k: Vec<f32> = if spec.sample_rate != target_rate {
            info!(
                "Resampling from {} Hz to {} Hz",
                spec.sample_rate, target_rate
            );
            resample_linear(&mono_samples, spec.sample_rate, target_rate)
        } else {
            mono_samples
        };

        let total_samples = samples_12k.len();
        let duration_s = total_samples as f64 / target_rate as f64;
        info!(
            "Audio ready: {} samples ({:.2}s) at {} Hz",
            total_samples, duration_s, target_rate
        );

        // Create FT8 decoder
        let ft8_config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(ft8_config)?;

        let window_size = pancetta_ft8::WINDOW_SAMPLES; // 151680 (12.64s @ 12 kHz)

        // Decode each 15-second slot worth of samples
        // FT8 windows overlap -- try decoding from multiple offsets
        let mut all_decoded = Vec::new();
        let mut offset = 0usize;

        // Step by half a window (6.32s) to catch messages at slot boundaries
        let step = window_size / 2;

        while offset + window_size <= total_samples {
            let window = &samples_12k[offset..offset + window_size];
            match decoder.decode_window(window) {
                Ok(messages) => {
                    for msg in &messages {
                        let freq_hz = msg.frequency_offset;
                        let snr = msg.snr_db;
                        let dt = msg.time_offset;
                        let text = &msg.text;

                        // Print in WSJT-X style format with confidence and AP level
                        let slot_time = offset as f64 / target_rate as f64;
                        let mins = (slot_time / 60.0) as u32;
                        let secs = (slot_time % 60.0) as u32;
                        let conf = msg.confidence;
                        let ap = msg.ap_level;
                        println!(
                            "{:02}:{:02}  {:>+4.0} {:>6.1} {:>+5.1}  conf={:.2} ap={}  {}",
                            mins, secs, snr, freq_hz, dt, conf, ap, text
                        );
                    }
                    all_decoded.extend(messages);
                }
                Err(e) => {
                    debug!("Decode error at offset {}: {}", offset, e);
                }
            }
            offset += step;
        }

        // Also try from offset 0 if we haven't covered it
        if total_samples >= window_size && step > 0 {
            // Already covered above
        }

        println!(
            "\n--- Decoded {} messages from {} ---",
            all_decoded.len(),
            wav_path.display()
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::int_sample_max_val;

    #[test]
    fn valid_pcm_widths_match_naive_scale() {
        for bits in [8u16, 16, 24, 32] {
            let expected = (1i64 << (bits - 1)) as f32;
            assert_eq!(int_sample_max_val(bits).unwrap(), expected, "bits={bits}");
        }
        // Spot-check boundary value 1.
        assert_eq!(int_sample_max_val(1).unwrap(), 1.0);
    }

    #[test]
    fn zero_bits_is_rejected_not_underflowed() {
        // Naive `1i64 << (0u16 - 1)` would underflow the u16 subtraction.
        assert!(int_sample_max_val(0).is_err());
    }

    #[test]
    fn oversized_bits_are_rejected_not_overflowed() {
        // Naive `1i64 << (65535 - 1)` would be an oversized shift.
        assert!(int_sample_max_val(33).is_err());
        assert!(int_sample_max_val(u16::MAX).is_err());
    }
}
