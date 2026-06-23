//! Batch 29 / Diagnostic J — hb-100 synthetic interferer-pair quick survey.
//!
//! Generate two FT8 signals at controlled (Δfreq, Δdt) separations and
//! amplitudes. Measure pancetta's ability to decode each individually
//! vs combined.
//!
//! Hypotheses tested:
//! * **hb-100** (interferer-pair corpus): characterize the decode-rate
//!   as a function of pair separation. Identifies the "shoulder of
//!   capture-effect" — Δfreq below which the stronger signal blocks the
//!   weaker.
//! * Provides corpus data for future joint-pair-retry / coherent-subtract
//!   work (which graduates have explored at length but production gains
//!   are bounded).
//!
//! Method:
//! 1. Generate signal A at base freq with amplitude 1.0.
//! 2. Generate signal B at offset {6.25, 12.5, 25, 50, 100} Hz, dt = 0,
//!    with amplitude {1.0, 0.5} (matched / 6 dB weaker).
//! 3. Combine A + B + noise (σ=0.05).
//! 4. Decode each individually + combined. Count successes per pair-config.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch29_interferer_pair

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

fn decode_texts(samples: &[f32], cfg: &Ft8Config) -> Result<Vec<String>> {
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(decoded.into_iter().map(|d| d.text).collect())
}

fn main() -> Result<()> {
    let sigma: f32 = std::env::var("BATCH29_J_SIGMA")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.05);
    let n_realizations: usize = std::env::var("BATCH29_J_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    println!(
        "## Batch 29 / Diagnostic J — interferer-pair separation survey ({} realizations per cell, noise σ={})",
        n_realizations, sigma
    );

    let mut encoder = Ft8Encoder::new();
    let mut modulator = Ft8Modulator::new_default()?;

    let msg_a = "CQ K5ARH EM10";
    let msg_b = "CQ W1XYZ FN42";
    let symbols_a = encoder
        .encode_message(msg_a, None)
        .map_err(|e| anyhow::anyhow!("encode A: {e}"))?;
    let symbols_b = encoder
        .encode_message(msg_b, None)
        .map_err(|e| anyhow::anyhow!("encode B: {e}"))?;

    // Both signals are modulated at base 1500 Hz (modulator's default
    // base_frequency); we pass frequency_offset = the relative offset.
    let mut signal_a = modulator
        .modulate_symbols(&symbols_a, 0.0)
        .map_err(|e| anyhow::anyhow!("modulate A: {e}"))?;
    signal_a.resize(WINDOW_SAMPLES, 0.0);

    let separations_hz: Vec<f64> = vec![6.25, 12.5, 25.0, 50.0, 100.0, 200.0];
    let b_amplitudes: Vec<f32> = vec![1.0, 0.5];

    let cfg = Ft8Config::default();

    println!("\n  Δfreq   Bamp  | a_alone | b_alone | combined | a_in_combined | b_in_combined");
    println!("  ------  ----  | ------- | ------- | -------- | ------------- | -------------");

    for &b_amp in &b_amplitudes {
        for &dfreq in &separations_hz {
            let mut signal_b = modulator
                .modulate_symbols(&symbols_b, dfreq)
                .map_err(|e| anyhow::anyhow!("modulate B at +{}: {}", dfreq, e))?;
            signal_b.resize(WINDOW_SAMPLES, 0.0);

            let mut a_alone_ok = 0;
            let mut b_alone_ok = 0;
            let mut combined_ok = 0;
            let mut a_in_combined = 0;
            let mut b_in_combined = 0;

            for r in 0..n_realizations {
                let mut rng = StdRng::seed_from_u64(8888 + r as u64);
                let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);

                let recv_a: Vec<f32> = signal_a.iter().zip(&noise).map(|(s, n)| s + n).collect();
                let recv_b: Vec<f32> = signal_b
                    .iter()
                    .zip(&noise)
                    .map(|(s, n)| b_amp * s + n)
                    .collect();
                let recv_combined: Vec<f32> = signal_a
                    .iter()
                    .zip(signal_b.iter())
                    .zip(&noise)
                    .map(|((a, b), n)| a + b_amp * b + n)
                    .collect();

                let a_set = decode_texts(&recv_a, &cfg)?;
                let b_set = decode_texts(&recv_b, &cfg)?;
                let c_set = decode_texts(&recv_combined, &cfg)?;
                if a_set.iter().any(|t| t == msg_a) {
                    a_alone_ok += 1;
                }
                if b_set.iter().any(|t| t == msg_b) {
                    b_alone_ok += 1;
                }
                if !c_set.is_empty() {
                    combined_ok += 1;
                }
                if c_set.iter().any(|t| t == msg_a) {
                    a_in_combined += 1;
                }
                if c_set.iter().any(|t| t == msg_b) {
                    b_in_combined += 1;
                }
            }

            println!(
                "  {:>6.2}  {:.2}  | {:>3}/{:<3} | {:>3}/{:<3} | {:>4}/{:<3} | {:>4}/{:<8} | {:>4}/{:<3}",
                dfreq,
                b_amp,
                a_alone_ok,
                n_realizations,
                b_alone_ok,
                n_realizations,
                combined_ok,
                n_realizations,
                a_in_combined,
                n_realizations,
                b_in_combined,
                n_realizations
            );
        }
    }

    println!("\n### Verdict / corpus data");
    println!("  Generated interferer-pair decode-rate map across 6 Δfreq × 2 amplitudes × {} realizations.",
        n_realizations);
    println!("  PROCEED-INFO — characterizes capture-effect threshold for joint-pair-retry");
    println!("  hypothesis families (hb-079, hb-080, hb-086). Provides synthetic-corpus baseline");
    println!("  for future MRC/coherent-subtract diagnostics.");

    Ok(())
}
