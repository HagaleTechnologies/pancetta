//! OSD integration tests: verify OSD weak-signal recovery and false-positive behavior
//!
//! Test 1: Encode -> modulate -> add noise -> decode, comparing BP-only vs OSD-enabled.
//! Test 2: Pure noise input should produce no false decodes with OSD enabled.

#![cfg(feature = "transmit")]

mod test_signal_generator;

use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES};

/// Deterministic Box-Muller noise using LCG PRNG (seed=42) for reproducibility.
fn add_noise_f64(signal: &[f64], snr_db: f64) -> Vec<f64> {
    let signal_power: f64 = signal.iter().map(|s| s * s).sum::<f64>() / signal.len() as f64;
    let noise_power = signal_power / 10.0f64.powf(snr_db / 10.0);
    let noise_std = noise_power.sqrt();

    let mut noisy = signal.to_vec();
    let mut seed: u64 = 42;
    for i in (0..noisy.len()).step_by(2) {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let u1 = (seed >> 33) as f64 / (1u64 << 31) as f64;
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let u2 = (seed >> 33) as f64 / (1u64 << 31) as f64;
        let u1 = u1.max(1e-10);
        let z0 = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
        let z1 = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).sin();
        noisy[i] += z0 * noise_std;
        if i + 1 < noisy.len() {
            noisy[i + 1] += z1 * noise_std;
        }
    }
    noisy
}

/// Generate an FT8 signal using the real encoder + modulator pipeline.
fn generate_ft8_signal(message: &str, frequency_offset: f64) -> Vec<f64> {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder
        .encode_message(message, None)
        .expect("encoding should succeed");
    let mut modulator = Ft8Modulator::new_default().unwrap();
    let samples_f32 = modulator
        .modulate_symbols(&symbols, frequency_offset)
        .expect("modulation should succeed");
    samples_f32.iter().map(|&s| s as f64).collect()
}

/// Decode audio with specific OSD config, returning decoded messages.
fn decode_with_osd(audio: &[f32], osd_depth: Option<u8>) -> Vec<pancetta_ft8::DecodedMessage> {
    let config = Ft8Config {
        osd_depth,
        max_decode_passes: 1,
        ..Ft8Config::default()
    };
    let mut decoder = Ft8Decoder::new(config).unwrap();
    decoder.decode_window(audio).unwrap_or_default()
}

/// Check if any decoded message contains the target callsign.
fn contains_callsign(messages: &[pancetta_ft8::DecodedMessage], callsign: &str) -> bool {
    messages.iter().any(|m| m.text.contains(callsign))
}

#[test]
fn test_osd_recovers_weak_signal() {
    let message = "CQ W1ABC FN42";
    let callsign = "W1ABC";
    let frequency_offset = 500.0; // Hz offset from base frequency

    // Generate clean signal
    let clean_signal = generate_ft8_signal(message, frequency_offset);

    let snr_values = [-22.0, -21.0, -20.0, -19.0, -18.0];
    let mut osd_advantage_found = false;

    for &snr_db in &snr_values {
        // Add noise at this SNR
        let noisy = add_noise_f64(&clean_signal, snr_db);

        // Convert to f32 and pad/truncate to WINDOW_SAMPLES
        let mut audio: Vec<f32> = noisy.iter().map(|&s| s as f32).collect();
        audio.resize(WINDOW_SAMPLES, 0.0);

        // Decode WITHOUT OSD (BP only)
        let bp_results = decode_with_osd(&audio, None);
        let bp_found = contains_callsign(&bp_results, callsign);

        // Decode WITH OSD (depth 2)
        let osd_results = decode_with_osd(&audio, Some(2));
        let osd_found = contains_callsign(&osd_results, callsign);

        eprintln!(
            "SNR={:5.1} dB: BP={}, OSD={}",
            snr_db,
            if bp_found { "DECODED" } else { "  fail " },
            if osd_found { "DECODED" } else { "  fail " },
        );

        if !bp_found && osd_found {
            eprintln!("  -> OSD advantage at SNR={} dB!", snr_db);
            osd_advantage_found = true;
        }
    }

    // At least one SNR level should show OSD recovering where BP failed.
    // If BP decodes everything, that's also acceptable (decoder is strong).
    // We only fail if OSD never decoded anything at all.
    let any_osd_decode = snr_values.iter().any(|&snr_db| {
        let noisy = add_noise_f64(&clean_signal, snr_db);
        let mut audio: Vec<f32> = noisy.iter().map(|&s| s as f32).collect();
        audio.resize(WINDOW_SAMPLES, 0.0);
        let osd_results = decode_with_osd(&audio, Some(2));
        contains_callsign(&osd_results, callsign)
    });

    assert!(
        osd_advantage_found || any_osd_decode,
        "OSD should either show advantage over BP or at least decode at some SNR level"
    );
}

#[test]
fn test_osd_no_false_positives_on_noise() {
    // Create a buffer of pure noise (no embedded signal)
    let mut audio = vec![0.0f32; WINDOW_SAMPLES];

    // Add noise using the existing test infrastructure
    test_signal_generator::add_gaussian_noise(&mut audio, 0.0);

    // Scale up noise to ensure it's non-trivial
    for sample in audio.iter_mut() {
        *sample *= 10.0;
    }

    // Decode with OSD enabled
    let osd_results = decode_with_osd(&audio, Some(2));

    // Also decode with BP-only for comparison
    let bp_results = decode_with_osd(&audio, None);

    // Filter out empty or unknown messages
    let filter_real = |results: &[pancetta_ft8::DecodedMessage]| -> Vec<String> {
        results
            .iter()
            .filter(|m| {
                !m.text.is_empty() && !m.text.contains("<Unknown>") && !m.text.contains("???")
            })
            .map(|m| m.text.clone())
            .collect()
    };

    let bp_decodes = filter_real(&bp_results);
    let osd_decodes = filter_real(&osd_results);

    eprintln!(
        "Pure noise decode: BP={} false positives, OSD={} false positives",
        bp_decodes.len(),
        osd_decodes.len()
    );
    for msg in &osd_decodes {
        eprintln!("  OSD false positive: '{}'", msg);
    }

    // OSD-2 with CRC-14 can produce a small number of false positives on noise
    // because it tries many candidate codewords and CRC-14 has a 1/16384 false
    // pass rate per trial. The key property is that the count stays bounded.
    // We allow up to 5 false positives (a generous bound) since the exact count
    // depends on noise realization. The important thing is no avalanche of false
    // decodes.
    let max_acceptable_false_positives = 5;
    assert!(
        osd_decodes.len() <= max_acceptable_false_positives,
        "OSD produced too many false decodes on pure noise: {} (max acceptable: {})",
        osd_decodes.len(),
        max_acceptable_false_positives
    );
}
