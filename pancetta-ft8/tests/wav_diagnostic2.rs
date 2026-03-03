//! Diagnostic 2: frequency sweep at known timing offset.

use pancetta_ft8::{SAMPLE_RATE, NUM_SYMBOLS, NUM_TONES, TONE_SPACING, SYMBOL_DURATION};

fn read_wav_file(path: &str) -> Vec<f32> {
    let reader = hound::WavReader::open(path).unwrap();
    reader.into_samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect()
}

fn fixture(subpath: &str) -> String {
    format!("{}/tests/fixtures/wav/{}", env!("CARGO_MANIFEST_DIR"), subpath)
}

/// Sweep base frequency at optimal timing to find actual signal positions.
/// This tells us WHERE real FT8 signals are in the recording.
#[test]
fn test_frequency_sweep_costas() {
    use std::f64::consts::PI;

    let path = fixture("basicft8/170923_082000.wav");
    let samples = read_wav_file(&path);
    let audio: Vec<f64> = samples.iter().map(|&s| s as f64).collect();

    let max_amp = audio.iter().fold(0.0f64, |acc, &x| acc.max(x.abs()));
    let audio: Vec<f64> = audio.iter().map(|&s| s * 0.95 / max_amp).collect();

    let costas = [3u8, 1, 4, 0, 6, 5, 2];
    let sps = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;
    let pi2 = 2.0 * PI;
    let dt = 1.0 / SAMPLE_RATE as f64;

    let window: Vec<f64> = (0..sps)
        .map(|i| 0.5 * (1.0 - (pi2 * i as f64 / (sps - 1) as f64).cos()))
        .collect();

    // Search all time offsets (every 192 samples = 1/10 symbol) and all frequencies
    // (every 6.25 Hz from 200 to 3000 Hz).
    println!("Sweeping time and frequency...");

    let mut results: Vec<(usize, f64, usize)> = Vec::new(); // (offset, base_freq, costas_correct)

    for offset_step in 0..120 { // 0 to ~11520 samples (0 to ~1s)
        let offset = offset_step * 96;
        let end = offset + NUM_SYMBOLS * sps;
        if end > audio.len() { break; }

        // Use sub-bin frequency resolution: step every 3.125 Hz (half-bin)
        for freq_step in (400..4800).step_by(10) { // 200 to 2400 Hz in 5 Hz steps
            let base_freq = freq_step as f64 * 0.5;

            // Extract just the Costas symbols and check
            let mut correct = 0;
            for &group_start in &[0usize, 36, 72] {
                for j in 0..7 {
                    let sym_idx = group_start + j;
                    let sym_start = offset + sym_idx * sps;
                    let symbol_audio = &audio[sym_start..sym_start + sps];

                    let mut best_tone = 0u8;
                    let mut best_mag = 0.0f64;
                    for tone in 0..NUM_TONES {
                        let freq = base_freq + tone as f64 * TONE_SPACING;
                        let mut re = 0.0;
                        let mut im = 0.0;
                        for (i, &s) in symbol_audio.iter().enumerate() {
                            let w = window[i];
                            let phase = pi2 * freq * i as f64 * dt;
                            re += s * w * phase.cos();
                            im += s * w * phase.sin();
                        }
                        let mag = (re * re + im * im).sqrt();
                        if mag > best_mag {
                            best_mag = mag;
                            best_tone = tone as u8;
                        }
                    }

                    if best_tone == costas[j] {
                        correct += 1;
                    }
                }
            }

            if correct >= 15 { // 15/21 = 71%
                results.push((offset, base_freq, correct));
            }
        }
    }

    // Sort by Costas correctness
    results.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));

    println!("\nTop Costas matches (>= 15/21):");
    for (offset, freq, correct) in results.iter().take(30) {
        println!("  offset={:5} ({:.4}s) freq={:7.2} Hz: {}/21 correct",
                 offset, *offset as f64 / SAMPLE_RATE as f64, freq, correct);
    }

    if results.is_empty() {
        println!("No positions found with >= 15/21 Costas correct.");
        println!("Trying >= 10/21...");

        let mut results10: Vec<(usize, f64, usize)> = Vec::new();
        for offset_step in 0..120 {
            let offset = offset_step * 96;
            let end = offset + NUM_SYMBOLS * sps;
            if end > audio.len() { break; }

            for freq_step in (400..4800).step_by(10) {
                let base_freq = freq_step as f64 * 0.5;
                let mut correct = 0;
                for &group_start in &[0usize, 36, 72] {
                    for j in 0..7 {
                        let sym_idx = group_start + j;
                        let sym_start = offset + sym_idx * sps;
                        let symbol_audio = &audio[sym_start..sym_start + sps];
                        let mut best_tone = 0u8;
                        let mut best_mag = 0.0f64;
                        for tone in 0..NUM_TONES {
                            let freq = base_freq + tone as f64 * TONE_SPACING;
                            let mut re = 0.0;
                            let mut im = 0.0;
                            for (i, &s) in symbol_audio.iter().enumerate() {
                                let w = window[i];
                                let phase = pi2 * freq * i as f64 * dt;
                                re += s * w * phase.cos();
                                im += s * w * phase.sin();
                            }
                            let mag = (re * re + im * im).sqrt();
                            if mag > best_mag { best_mag = mag; best_tone = tone as u8; }
                        }
                        if best_tone == costas[j] { correct += 1; }
                    }
                }
                if correct >= 10 {
                    results10.push((offset, base_freq, correct));
                }
            }
        }
        results10.sort_by(|a, b| b.2.cmp(&a.2));
        for (offset, freq, correct) in results10.iter().take(20) {
            println!("  offset={:5} ({:.4}s) freq={:7.2} Hz: {}/21 correct",
                     offset, *offset as f64 / SAMPLE_RATE as f64, freq, correct);
        }
    }
}
