//! Diagnostic test to understand why off-air WAV files produce 0 decodes.

use pancetta_ft8::{Ft8Decoder, Ft8Config, WINDOW_SAMPLES, SAMPLE_RATE, NUM_SYMBOLS, NUM_TONES, TONE_SPACING, SYMBOL_DURATION};

fn read_wav_file(path: &str) -> Vec<f32> {
    let reader = hound::WavReader::open(path).unwrap();
    reader
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect()
}

fn fixture(subpath: &str) -> String {
    format!(
        "{}/tests/fixtures/wav/{}",
        env!("CARGO_MANIFEST_DIR"),
        subpath
    )
}

/// Manually extract symbols at a given time/freq position and check
/// if the Costas sync pattern is correct.
#[test]
fn test_symbol_extraction_quality() {
    use std::f64::consts::PI;

    let path = fixture("basicft8/170923_082000.wav");
    let samples = read_wav_file(&path);
    let audio: Vec<f64> = samples.iter().map(|&s| s as f64).collect();

    // Normalize like the decoder does
    let max_amp = audio.iter().fold(0.0f64, |acc, &x| acc.max(x.abs()));
    let audio: Vec<f64> = audio.iter().map(|&s| s * 0.95 / max_amp).collect();

    let costas = [3u8, 1, 4, 0, 6, 5, 2];
    let samples_per_symbol = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize; // 1920

    // The best sync candidate was at t=6, f=169 (1056.25 Hz)
    // Try a range of sample offsets near t=6 * 960 = 5760
    let base_freq = 169.0 * TONE_SPACING; // 1056.25 Hz
    let pi2 = 2.0 * PI;
    let dt = 1.0 / SAMPLE_RATE as f64;

    println!("Extracting symbols at base_freq={:.2} Hz", base_freq);
    println!("Costas expected: {:?}", costas);

    // Hann window
    let window: Vec<f64> = (0..samples_per_symbol)
        .map(|i| 0.5 * (1.0 - (pi2 * i as f64 / (samples_per_symbol - 1) as f64).cos()))
        .collect();

    // Try offsets from 4800 to 7200 in steps of 96 (1/20 symbol)
    let mut best_offset = 0;
    let mut best_costas_correct = 0;

    for offset in (4800..=7200).step_by(96) {
        let end = offset + NUM_SYMBOLS * samples_per_symbol;
        if end > audio.len() { break; }

        // Extract all 79 symbols
        let mut symbols = Vec::with_capacity(NUM_SYMBOLS);
        for sym_idx in 0..NUM_SYMBOLS {
            let sym_start = offset + sym_idx * samples_per_symbol;
            let symbol_audio = &audio[sym_start..sym_start + samples_per_symbol];

            let mut best_tone = 0u8;
            let mut best_mag = 0.0f64;

            for tone in 0..NUM_TONES {
                let freq = base_freq + tone as f64 * TONE_SPACING;
                let mut real_sum = 0.0;
                let mut imag_sum = 0.0;
                for (i, &sample) in symbol_audio.iter().enumerate() {
                    let w = window[i];
                    let phase = pi2 * freq * i as f64 * dt;
                    real_sum += sample * w * phase.cos();
                    imag_sum += sample * w * phase.sin();
                }
                let magnitude = (real_sum * real_sum + imag_sum * imag_sum).sqrt();
                if magnitude > best_mag {
                    best_mag = magnitude;
                    best_tone = tone as u8;
                }
            }
            symbols.push(best_tone);
        }

        // Check Costas at positions 0-6, 36-42, 72-78
        let mut correct = 0;
        for (i, &expected) in costas.iter().enumerate() {
            if symbols[i] == expected { correct += 1; }
            if symbols[36 + i] == expected { correct += 1; }
            if symbols[72 + i] == expected { correct += 1; }
        }

        if correct > best_costas_correct {
            best_costas_correct = correct;
            best_offset = offset;
        }

        if correct >= 18 { // 18/21 = 86% correct
            println!("\nOffset={} ({:.4}s): {}/21 Costas correct", offset, offset as f64 / SAMPLE_RATE as f64, correct);
            println!("  First Costas:  {:?} (expected {:?})", &symbols[0..7], costas);
            println!("  Second Costas: {:?} (expected {:?})", &symbols[36..43], costas);
            println!("  Third Costas:  {:?} (expected {:?})", &symbols[72..79], costas);
        }
    }

    println!("\nBest offset={} ({:.4}s): {}/21 Costas correct",
             best_offset, best_offset as f64 / SAMPLE_RATE as f64, best_costas_correct);

    // Show the best offset's symbols
    let end = best_offset + NUM_SYMBOLS * samples_per_symbol;
    if end <= audio.len() {
        let mut symbols = Vec::with_capacity(NUM_SYMBOLS);
        let mut tone_snrs = Vec::with_capacity(NUM_SYMBOLS);
        for sym_idx in 0..NUM_SYMBOLS {
            let sym_start = best_offset + sym_idx * samples_per_symbol;
            let symbol_audio = &audio[sym_start..sym_start + samples_per_symbol];

            let mut mags = [0.0f64; 8];
            let mut best_tone = 0u8;
            let mut best_mag = 0.0f64;

            for tone in 0..NUM_TONES {
                let freq = base_freq + tone as f64 * TONE_SPACING;
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

            // SNR of best tone vs average of others
            let noise_avg: f64 = mags.iter().enumerate()
                .filter(|(i, _)| *i != best_tone as usize)
                .map(|(_, &m)| m).sum::<f64>() / 7.0;
            let snr = if noise_avg > 0.0 { best_mag / noise_avg } else { 99.0 };

            symbols.push(best_tone);
            tone_snrs.push(snr);
        }

        println!("\nSymbol details at best offset:");
        println!("  First Costas:  {:?}", &symbols[0..7]);
        println!("  Expected:      {:?}", costas);
        for i in 0..7 {
            let ok = if symbols[i] == costas[i] { "OK" } else { "WRONG" };
            println!("    [{}] tone={} expected={} SNR={:.1} {}", i, symbols[i], costas[i], tone_snrs[i], ok);
        }
        println!("  Second Costas: {:?}", &symbols[36..43]);
        println!("  Third Costas:  {:?}", &symbols[72..79]);

        // Summary: how many data symbols have SNR > 2?
        let data_indices: Vec<usize> = (7..36).chain(43..72).collect();
        let good_snrs = data_indices.iter().filter(|&&i| tone_snrs[i] > 2.0).count();
        let avg_data_snr: f64 = data_indices.iter().map(|&i| tone_snrs[i]).sum::<f64>() / data_indices.len() as f64;
        println!("\n  Data symbols: {}/58 with SNR>2, avg SNR={:.2}", good_snrs, avg_data_snr);
    }
}

/// Test the decoder with the full 15s buffer.
#[test]
fn test_full_buffer_decode() {
    let path = fixture("basicft8/170923_082000.wav");
    let samples = read_wav_file(&path);

    let mut config = Ft8Config::default();
    config.aggressive_decoding = true;
    config.min_snr_db = -30.0;
    config.max_candidates = 200;

    let mut decoder = Ft8Decoder::new(config).unwrap();
    let decoded = decoder.decode_window(&samples).unwrap_or_default();
    let metrics = decoder.get_last_metrics();

    println!("Full buffer ({}): {} messages, sync_quality={:.3}, time={:?}",
             samples.len(), decoded.len(), metrics.sync_quality, metrics.processing_time);

    for m in &decoded {
        println!("  [{:6.1} dB] {}", m.snr_db, m.text);
    }
}
