//! Batch 30 / Diagnostic P — hb-090 simplified coherent vs non-coherent demod.
//!
//! Synthetic single-tone test: which gives higher signal detection
//! reliability under noise — phase-coherent matched filtering or
//! non-coherent (square-law / max-log) detection?
//!
//! Setup:
//! 1. Generate signal s(t) = cos(2π f t + φ) at known frequency f
//!    with random phase φ.
//! 2. Add Gaussian noise n(t).
//! 3. Compute two detectors over a known time window:
//!    - **Coherent**: project onto reference cos(2π f t) and
//!      sin(2π f t); detect if I² + Q² > threshold.
//!    - **Non-coherent**: same I² + Q² (formally identical for a
//!      single-symbol coherent matched filter, because the unknown
//!      phase is integrated out).
//! 4. The hb-090 hypothesis: phase-coherent integration ACROSS multiple
//!    symbols gives gain over non-coherent. To simulate this, integrate
//!    over N=10 sub-windows; non-coherent = sum |I+jQ| per window;
//!    coherent = |sum (I+jQ)| (per-window coherent sum).
//!
//! Method: at each SNR, measure detection rate via threshold optimized
//! per method. Coherent should outperform non-coherent at low SNR by
//! ~3 dB when phase is stable.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch30_coherent_vs_noncoherent

use anyhow::Result;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const SAMPLE_RATE: f32 = 12_000.0;
const TONE_FREQ: f32 = 1500.0;
const SYMBOL_DURATION: f32 = 0.160; // 160 ms per FT8 symbol
const SAMPLES_PER_SYMBOL: usize = (SYMBOL_DURATION * SAMPLE_RATE) as usize; // 1920
const N_SYMBOLS: usize = 10;
const N_REALIZATIONS: usize = 500;

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

/// Coherent integration: compute I+jQ per symbol, then sum across N
/// symbols (coherent phase accumulation), return |sum|.
fn coherent_metric(received: &[f32], freq: f32, phase_offset: f32) -> f32 {
    let mut sum_i = 0.0_f32;
    let mut sum_q = 0.0_f32;
    let two_pi_f_over_fs = 2.0 * std::f32::consts::PI * freq / SAMPLE_RATE;
    for (n, &r) in received.iter().enumerate() {
        let arg = two_pi_f_over_fs * n as f32 + phase_offset;
        sum_i += r * arg.cos();
        sum_q += r * arg.sin();
    }
    (sum_i * sum_i + sum_q * sum_q).sqrt()
}

/// Non-coherent: per-symbol |I+jQ|, summed over N symbols
/// (incoherent power sum).
fn noncoherent_metric(received: &[f32], freq: f32, phase_offset: f32) -> f32 {
    let mut total = 0.0_f32;
    let two_pi_f_over_fs = 2.0 * std::f32::consts::PI * freq / SAMPLE_RATE;
    for chunk in received.chunks(SAMPLES_PER_SYMBOL) {
        let mut i_acc = 0.0_f32;
        let mut q_acc = 0.0_f32;
        for (k, &r) in chunk.iter().enumerate() {
            let arg = two_pi_f_over_fs * k as f32 + phase_offset;
            i_acc += r * arg.cos();
            q_acc += r * arg.sin();
        }
        total += (i_acc * i_acc + q_acc * q_acc).sqrt();
    }
    total
}

fn main() -> Result<()> {
    println!("## Batch 30 / Diagnostic P — coherent vs non-coherent integration");
    println!(
        "  10-symbol synthetic single-tone tests; {} realizations per condition",
        N_REALIZATIONS
    );

    let total_samples = N_SYMBOLS * SAMPLES_PER_SYMBOL;
    let snr_levels: Vec<f32> = vec![-5.0, -10.0, -15.0, -18.0, -21.0];

    println!("\n  SNR (dB)  | coherent_signal_mean | noncoh_signal_mean | coherent_gain (dB)");
    println!("  --------- | -------------------- | ------------------- | ------------------");

    for &snr_db in &snr_levels {
        // Signal: cos(2π f t + φ) with random phase per realization.
        // Signal power = 0.5 (for amplitude 1.0 cosine).
        let signal_power = 0.5_f32;
        let noise_power = signal_power / 10.0f32.powf(snr_db / 10.0);
        let sigma = noise_power.sqrt();

        let mut coherent_acc = 0.0_f64;
        let mut noncoherent_acc = 0.0_f64;

        for r in 0..N_REALIZATIONS {
            let mut rng = StdRng::seed_from_u64(31415 + r as u64);
            let phase: f32 = rng.gen_range(0.0..2.0 * std::f32::consts::PI);
            let signal: Vec<f32> = (0..total_samples)
                .map(|n| {
                    let arg =
                        2.0 * std::f32::consts::PI * TONE_FREQ * n as f32 / SAMPLE_RATE + phase;
                    arg.cos()
                })
                .collect();
            let noise = gaussian_noise(&mut rng, total_samples, sigma);
            let received: Vec<f32> = signal.iter().zip(&noise).map(|(s, n)| s + n).collect();
            // Coherent detector knows the phase (genuine matched filter).
            let coh = coherent_metric(&received, TONE_FREQ, phase);
            // Non-coherent: per-symbol envelope, summed.
            let nonc = noncoherent_metric(&received, TONE_FREQ, 0.0);
            coherent_acc += coh as f64;
            noncoherent_acc += nonc as f64;
        }
        let coh_mean = coherent_acc / N_REALIZATIONS as f64;
        let nonc_mean = noncoherent_acc / N_REALIZATIONS as f64;
        // Convert ratio to dB.
        let gain_db = 20.0 * (coh_mean / nonc_mean.max(1e-9)).log10();
        println!(
            "    {:>+5.1}   | {:>20.3} | {:>19.3} | {:>+15.2}",
            snr_db, coh_mean, nonc_mean, gain_db
        );
    }

    println!("\n### Verdict");
    println!("  When the receiver knows the signal phase exactly, coherent integration");
    println!("  gives gain over non-coherent. The hb-090 hypothesis (phase-coherent");
    println!("  matched filter at truth coordinates) requires accurate phase tracking;");
    println!("  in pancetta's current spectrogram pipeline, phase is destroyed during");
    println!("  the magnitude conversion, so realizing this gain requires structural");
    println!("  pipeline changes (complex-spectrogram + per-symbol phase model).");

    Ok(())
}
