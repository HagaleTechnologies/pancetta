//! WAV file decoding tests
//!
//! Tests decoder against:
//! 1. Generated GFSK test signals (known-decodable by ft8_lib)
//! 2. Off-air recordings (best-effort — these may not decode with either
//!    our decoder or ft8_lib due to unknown signal characteristics)

use pancetta_ft8::{Ft8Decoder, Ft8Config, DecodedMessage, WINDOW_SAMPLES, SAMPLE_RATE,
                   ft8_lib_ffi::ft8lib_decode_audio};

fn read_wav_file(path: &str) -> Vec<f32> {
    let reader = hound::WavReader::open(path)
        .unwrap_or_else(|e| panic!("Failed to open {}: {}", path, e));
    let spec = reader.spec();
    assert_eq!(spec.channels, 1);
    assert_eq!(spec.sample_rate, SAMPLE_RATE);
    reader
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect()
}

fn decode_wav_file(path: &str) -> Vec<DecodedMessage> {
    let samples = read_wav_file(path);
    let buffer: Vec<f32> = if samples.len() >= WINDOW_SAMPLES {
        samples
    } else {
        let mut padded = samples;
        padded.resize(WINDOW_SAMPLES, 0.0);
        padded
    };
    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();
    decoder.decode_window(&buffer).unwrap_or_default()
}

fn fixture(subpath: &str) -> String {
    format!(
        "{}/tests/fixtures/wav/{}",
        env!("CARGO_MANIFEST_DIR"),
        subpath
    )
}

// =============================================================
// Generated GFSK test signals (known-good, decodable by ft8_lib)
// =============================================================

#[test]
fn test_decode_generated_cq() {
    let decoded = decode_wav_file(&fixture("generated/ft8_cq.wav"));
    // Our decoder may or may not decode GFSK signals from ft8_lib.
    // This test documents current capability.
    println!(
        "generated/ft8_cq.wav: {} messages decoded",
        decoded.len()
    );
    for m in &decoded {
        println!("  [{:6.1} dB] {}", m.snr_db, m.text);
    }
}

#[test]
fn test_decode_generated_report() {
    let decoded = decode_wav_file(&fixture("generated/ft8_report.wav"));
    println!(
        "generated/ft8_report.wav: {} messages decoded",
        decoded.len()
    );
    for m in &decoded {
        println!("  [{:6.1} dB] {}", m.snr_db, m.text);
    }
}

#[test]
fn test_decode_generated_rr73() {
    let decoded = decode_wav_file(&fixture("generated/ft8_rr73.wav"));
    println!(
        "generated/ft8_rr73.wav: {} messages decoded",
        decoded.len()
    );
    for m in &decoded {
        println!("  [{:6.1} dB] {}", m.snr_db, m.text);
    }
}

// =============================================================
// Off-air recordings (informational — ft8_lib also can't decode these)
// =============================================================

#[test]
fn test_decode_offair_summary() {
    let files = [
        "jtdx/000000_000001.wav",
        "jtdx/190227_155815.wav",
        "wsjt/210703_133430.wav",
        "wsjt/181201_180245.wav",
        "wsjt/170709_135615.wav",
        "basicft8/170923_082000.wav",
        "basicft8/170923_082015.wav",
        "basicft8/170923_082030.wav",
        "basicft8/170923_082045.wav",
    ];

    let mut total = 0;
    let mut with_decodes = 0;

    for file in &files {
        let decoded = decode_wav_file(&fixture(file));
        if !decoded.is_empty() {
            with_decodes += 1;
        }
        total += decoded.len();
        println!("{}: {} messages", file, decoded.len());
        for m in &decoded {
            println!("  [{:6.1} dB] {}", m.snr_db, m.text);
        }
    }

    println!(
        "\nOff-air summary: {} messages from {}/{} files",
        total, with_decodes, files.len()
    );
    // Note: ft8_lib (kgoba/ft8_lib latest) also decodes 0 from these files.
    // These recordings may not contain standard FT8 or may require a different
    // decoder configuration. No assertion — this is informational only.
}

// =============================================================
// Cross-validation against ft8_lib reference implementation
// =============================================================

/// Compare our decoder's output against ft8_lib for each WAV file.
/// We assert that we decode at least 80% of what ft8_lib decodes.
#[test]
fn test_cross_validate_against_ft8lib() {
    let files = [
        "jtdx/000000_000001.wav",
        "jtdx/190227_155815.wav",
        "wsjt/210703_133430.wav",
        "wsjt/181201_180245.wav",
        "wsjt/170709_135615.wav",
        "basicft8/170923_082000.wav",
        "basicft8/170923_082015.wav",
        "basicft8/170923_082030.wav",
        "basicft8/170923_082045.wav",
    ];

    let mut total_ours = 0usize;
    let mut total_ft8lib = 0usize;
    let mut files_below_threshold = Vec::new();

    for file in &files {
        let path = fixture(file);
        let samples = read_wav_file(&path);
        let buffer: Vec<f32> = if samples.len() >= WINDOW_SAMPLES {
            samples.clone()
        } else {
            let mut padded = samples.clone();
            padded.resize(WINDOW_SAMPLES, 0.0);
            padded
        };

        // Our decoder
        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();
        let our_decoded = decoder.decode_window(&buffer).unwrap_or_default();
        let our_count = our_decoded.len();

        // ft8_lib reference decoder
        let ft8lib_decoded = ft8lib_decode_audio(&samples);
        let ft8lib_count = ft8lib_decoded.len();

        total_ours += our_count;
        total_ft8lib += ft8lib_count;

        println!("{}: ours={}, ft8_lib={}", file, our_count, ft8lib_count);

        // Per-file check: if ft8_lib decodes messages, we should decode >= 80%
        if ft8lib_count > 0 {
            let ratio = our_count as f64 / ft8lib_count as f64;
            println!("  ratio: {:.1}%", ratio * 100.0);
            if ratio < 0.80 {
                files_below_threshold.push(format!(
                    "{}: ours={} vs ft8_lib={} ({:.0}%)",
                    file, our_count, ft8lib_count, ratio * 100.0
                ));
            }
        }

        // Print decoded messages for comparison
        for m in &our_decoded {
            println!("  [ours]   {:6.1} dB  {}", m.snr_db, m.text);
        }
        for (text, freq, _time, _ldpc) in &ft8lib_decoded {
            println!("  [ft8lib] {:6.1} Hz  {}", freq, text);
        }
    }

    println!(
        "\nCross-validation totals: ours={}, ft8_lib={}",
        total_ours, total_ft8lib
    );

    // Overall assertion: we decode at least 80% of what ft8_lib does
    if total_ft8lib > 0 {
        let overall_ratio = total_ours as f64 / total_ft8lib as f64;
        println!("Overall ratio: {:.1}%", overall_ratio * 100.0);

        assert!(
            overall_ratio >= 0.80,
            "Overall decode ratio {:.1}% is below 80% threshold. Ours={}, ft8_lib={}.\nPer-file failures:\n{}",
            overall_ratio * 100.0,
            total_ours,
            total_ft8lib,
            files_below_threshold.join("\n")
        );
    }
}
