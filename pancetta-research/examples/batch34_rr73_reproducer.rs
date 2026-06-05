//! Batch 34 / Phase 1A — RR73 minimal reproducer (hb-217).
//!
//! Synthesize one of the known-missed RR73 messages from hard-200,
//! modulate it at clean SNR + various noise levels, and check whether
//! pancetta decodes it. Compare with the round-trip test that already
//! works for "K1DEF W1ABC RR73" — different callsigns, same protocol.
//!
//! If clean-synthetic decodes the missed message → the real audio has
//! something different (probably not a code bug per se).
//! If clean-synthetic FAILS → there's a code bug specific to certain
//! RR73 callsign patterns.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch34_rr73_reproducer

use anyhow::Result;
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

fn gaussian_noise(rng: &mut StdRng, n: usize, sigma: f32) -> Vec<f32> {
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let u1: f32 = rng.gen_range(f32::EPSILON..1.0);
        let u2: f32 = rng.gen_range(0.0..1.0);
        let mag = (-2.0 * u1.ln()).sqrt();
        let z0 = mag * (2.0 * std::f32::consts::PI * u2).cos();
        let z1 = mag * (2.0 * std::f32::consts::PI * u2).sin();
        out.push(z0 * sigma);
        i += 1;
        if i < n {
            out.push(z1 * sigma);
            i += 1;
        }
    }
    out
}

fn try_one(msg: &str, abs_freq_hz: f64, sigma: f32) -> Result<bool> {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder
        .encode_message(msg, None)
        .map_err(|e| anyhow::anyhow!("encode {msg}: {e}"))?;
    // Use abs_freq_hz directly as the base so any frequency in 200..3000 Hz
    // works (default modulator base=1500 + offset capped at 2500 deviation).
    use pancetta_ft8::modulator::DEFAULT_TX_POWER;
    let mut modulator = Ft8Modulator::new(12_000, abs_freq_hz, DEFAULT_TX_POWER)
        .map_err(|e| anyhow::anyhow!("Ft8Modulator::new at base {abs_freq_hz}: {e}"))?;
    let mut audio = modulator
        .modulate_symbols(&symbols, 0.0)
        .map_err(|e| anyhow::anyhow!("modulate {msg}: {e}"))?;
    audio.resize(WINDOW_SAMPLES, 0.0);
    if sigma > 0.0 {
        let mut rng = StdRng::seed_from_u64(42);
        let noise = gaussian_noise(&mut rng, audio.len(), sigma);
        for (a, n) in audio.iter_mut().zip(noise.iter()) {
            *a += *n;
        }
    }

    let cfg = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(cfg).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(&audio)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(decoded.iter().any(|d| d.text == msg))
}

fn main() -> Result<()> {
    println!("## Batch 34 / Phase 1A — RR73 minimal reproducer");

    // Known-missed RR73 truths from hard-200 (Batch 33 DD audit, top SNRs).
    // Frequencies are ABSOLUTE Hz; the modulator's base_frequency is set
    // per-case so any 200..3000 Hz target works.
    let cases: &[(&str, f64)] = &[
        ("K1DEF W1ABC RR73", 1500.0),  // round-trip baseline (control)
        ("KJ5CHK N8PFK RR73", 2899.0), // sha 1bd3e87f, missed at +21 dB
        ("VA7HZ KJ4IHN RR73", 2787.0), // sha d572fe73, missed at +14 dB
        ("WB3FME KB9AVX RR73", 791.0), // sha 17c4b25a, missed at +14 dB
        ("WZ2T KY4WI RR73", 2709.0),   // sha df1bfa5d, missed at +14 dB
        ("KA3KQO KB9AVX RR73", 791.0), // sha 5ba100a8, missed at +13 dB
        ("KJ5DQB K2PS RR73", 818.0),   // sha dade3979, missed at +13 dB
        ("WA3RIJ WF1B RR73", 2840.0),  // sha cbd255b3, missed at +13 dB
    ];

    let sigmas = [0.0_f32, 0.001, 0.01, 0.05];

    println!("\n  Message                       | freq(Hz) | σ=0   | σ=0.001 | σ=0.01 | σ=0.05");
    println!("  ----------------------------- | -------- | ----- | ------- | ------ | ------");
    for (msg, abs_freq) in cases {
        let mut results = Vec::new();
        for &sigma in &sigmas {
            let ok = match try_one(msg, *abs_freq, sigma) {
                Ok(b) => {
                    if b {
                        "PASS"
                    } else {
                        "fail"
                    }
                }
                Err(e) => {
                    eprintln!("  ERR for {} @ {}: {}", msg, abs_freq, e);
                    "ERR "
                }
            };
            results.push(ok);
        }
        println!(
            "  {:<29} | {:>8.0} | {:>5} | {:>7} | {:>6} | {:>5}",
            msg, abs_freq, results[0], results[1], results[2], results[3]
        );
    }

    println!("\n  PASS at σ=0 indicates the message round-trips at clean SNR.");
    println!("  Failures at clean SNR would signal a code bug in the RR73 path.");

    Ok(())
}
