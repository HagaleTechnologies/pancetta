use anyhow::Result;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use std::path::{Path, PathBuf};
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

/// Read a WAV file, mixing to mono if it's multi-channel. Returns the mono
/// samples at the file's own native sample rate (no resampling here) plus
/// that rate, so callers can resample to whatever target they need.
///
/// A failed sample read (truncated file, malformed data chunk) is a hard
/// error naming the offending path, NOT a silently dropped sample. Silently
/// skipping bad samples shortens the decoded audio without telling anyone:
/// under `--replay` (`replay.rs`, `load_replay_samples`) every subsequent
/// corpus file is concatenated straight onto the shortened stream, collapsing
/// archive time and shifting the alignment of every FT8 frame after the
/// corruption while the run still exits 0. `--wav` single-file mode has the
/// milder version of the same bug — a quietly truncated decode reported as a
/// successful one — so both callers want the error.
pub(crate) fn read_wav_mono(path: &Path) -> Result<(Vec<f32>, u32)> {
    let reader = hound::WavReader::open(path)
        .map_err(|e| anyhow::anyhow!("Failed to open WAV file {}: {}", path.display(), e))?;

    let spec = reader.spec();
    info!(
        "WAV: {} channels, {} Hz, {:?}, {} bits",
        spec.channels, spec.sample_rate, spec.sample_format, spec.bits_per_sample
    );

    let sample_err = |index: usize, e: hound::Error| {
        anyhow::anyhow!(
            "Failed to read sample {} of WAV file {}: {} (file is truncated or corrupt)",
            index,
            path.display(),
            e
        )
    };

    let raw_samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let max_val = int_sample_max_val(spec.bits_per_sample)?;
            reader
                .into_samples::<i32>()
                .enumerate()
                .map(|(i, s)| s.map(|s| s as f32 / max_val).map_err(|e| sample_err(i, e)))
                .collect::<Result<Vec<f32>>>()?
        }
        hound::SampleFormat::Float => reader
            .into_samples::<f32>()
            .enumerate()
            .map(|(i, s)| s.map_err(|e| sample_err(i, e)))
            .collect::<Result<Vec<f32>>>()?,
    };

    let mono_samples: Vec<f32> = if spec.channels > 1 {
        let ch = spec.channels as usize;
        raw_samples
            .chunks(ch)
            .map(|frame| frame.iter().sum::<f32>() / ch as f32)
            .collect()
    } else {
        raw_samples
    };

    Ok((mono_samples, spec.sample_rate))
}

impl super::ApplicationCoordinator {
    /// Run WAV playback mode: read file, decode, print results, exit.
    pub(crate) async fn run_wav_playback(&self, wav_path: PathBuf) -> Result<()> {
        info!("WAV playback mode: {}", wav_path.display());

        let (mono_samples, native_rate) = read_wav_mono(&wav_path)?;
        info!("Read {} mono samples", mono_samples.len());

        // Resample to 12 kHz if needed
        let target_rate = pancetta_ft8::SAMPLE_RATE;
        let samples_12k: Vec<f32> = if native_rate != target_rate {
            info!("Resampling from {} Hz to {} Hz", native_rate, target_rate);
            resample_linear(&mono_samples, native_rate, target_rate)
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

        let mut all_decoded = Vec::new();
        let mut offset = 0usize;
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
    use super::{int_sample_max_val, read_wav_mono};

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

    #[test]
    fn read_wav_mono_mixes_stereo_and_reports_native_rate() {
        use std::io::Cursor;

        // Build a tiny 2-channel, 16-bit, 8000 Hz WAV in memory: left=1.0, right=-1.0
        // for every sample, so mono mix-down must produce 0.0 at every sample.
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stereo.wav");
        {
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            for _ in 0..100 {
                writer.write_sample(i16::MAX).unwrap();
                writer.write_sample(i16::MIN + 1).unwrap();
            }
            writer.finalize().unwrap();
        }

        let (mono, rate) = read_wav_mono(&path).unwrap();
        assert_eq!(rate, 8000);
        assert_eq!(mono.len(), 100);
        for s in mono {
            assert!(s.abs() < 0.001, "expected near-zero mix-down, got {s}");
        }
        let _ = Cursor::new(Vec::<u8>::new()); // silence unused-import if hound changes later
    }

    /// Round-8 regression: a truncated WAV must be a hard, path-naming error,
    /// not a silently shortened success.
    ///
    /// The old `filter_map(|s| s.ok())` dropped every failed sample and read
    /// on. Under `--replay` that shortens one corpus file in place and
    /// concatenates the next one straight onto the gap, collapsing archive
    /// time and shifting the alignment of every later FT8 frame -- while the
    /// run still exits 0.
    #[test]
    fn truncated_wav_errors_instead_of_silently_shortening() {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 12000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.wav");
        {
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            for i in 0..500i16 {
                writer.write_sample(i).unwrap();
            }
            writer.finalize().unwrap();
        }

        // Control: intact, the file reads back in full.
        let (intact, rate) = read_wav_mono(&path).unwrap();
        assert_eq!(rate, 12000);
        assert_eq!(intact.len(), 500);

        // Chop the tail off the data chunk. The header still declares 500
        // samples, so `hound` reports an error partway through the iterator.
        let full_len = std::fs::metadata(&path).unwrap().len();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(full_len - 200).unwrap();
        drop(file);

        let err = read_wav_mono(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("truncated.wav"),
            "error must name the offending file so an operator can find it, got: {msg}"
        );
        assert!(
            msg.contains("Failed to read sample"),
            "error must name the cause, got: {msg}"
        );
    }
}
