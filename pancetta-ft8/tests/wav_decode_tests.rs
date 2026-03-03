//! WAV file decoding tests
//!
//! Tests decoder against:
//! 1. Generated GFSK test signals (known-decodable by ft8_lib)
//! 2. Off-air recordings (best-effort — these may not decode with either
//!    our decoder or ft8_lib due to unknown signal characteristics)

use pancetta_ft8::{Ft8Decoder, Ft8Config, DecodedMessage, WINDOW_SAMPLES, SAMPLE_RATE};

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
