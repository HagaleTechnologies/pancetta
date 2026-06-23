//! Batch 30 / Diagnostic N — Synthetic SNR-recall curve.
//!
//! Encode a known FT8 message at base frequency, add Gaussian noise at
//! a sequence of SNRs targeting FT8's expected operating range (-25
//! to -10 dB SNR-in-2500-Hz). Measure decode success rate. Establishes
//! pancetta's reference sensitivity curve under controlled conditions.
//!
//! Method:
//! 1. Encode "CQ K5ARH EM10" + modulate at 0 Hz offset (1500 Hz base).
//! 2. For each target SNR_dB (in 2500 Hz reference bandwidth):
//!    - Compute noise σ such that 10 log10(signal_power / noise_power_in_2500hz)
//!      = SNR_dB.
//!    - Add noise; decode; check if "CQ K5ARH EM10" is in the result.
//! 3. Repeat N times per SNR; report success rate.
//!
//! Notes:
//! * FT8 nominal sensitivity is ~-21 dB SNR in 2500 Hz bandwidth.
//! * pancetta-ft8's expected curve: ~100% at -15 dB, ~50% near -21 dB,
//!   ~0% below -25 dB.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch30_snr_recall_curve

use anyhow::Result;
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const SAMPLE_RATE: f32 = 12_000.0;

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

fn signal_power(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32
}

/// Compute σ for Gaussian noise such that 10*log10(P_signal / P_noise_in_BW) = snr_db,
/// where P_noise_in_BW = σ² * BW / (Fs/2). Bandwidth reference = 2500 Hz.
/// σ² = P_signal / (10^(snr_db/10)) * (Fs/2) / BW
fn sigma_for_snr_db(p_signal: f32, snr_db: f32) -> f32 {
    let bw: f32 = 2500.0;
    let p_n_in_bw = p_signal / 10.0f32.powf(snr_db / 10.0);
    let nyquist = SAMPLE_RATE / 2.0;
    let sigma2 = p_n_in_bw * nyquist / bw;
    sigma2.sqrt()
}

fn main() -> Result<()> {
    let n_realizations: usize = std::env::var("BATCH30_N_REALIZATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let snrs: Vec<f32> = std::env::var("BATCH30_N_SNRS")
        .unwrap_or_else(|_| "-10,-14,-18,-21,-25,-28".to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    println!("## Batch 30 / Diagnostic N — synthetic SNR-recall curve");
    println!(
        "  {} realizations per SNR; SNR levels (in 2500 Hz BW) {:?}",
        n_realizations, snrs
    );

    let mut encoder = Ft8Encoder::new();
    let target_msg = "CQ K5ARH EM10";
    let symbols = encoder
        .encode_message(target_msg, None)
        .map_err(|e| anyhow::anyhow!("encode: {e}"))?;
    let mut modulator = Ft8Modulator::new_default()?;
    let mut clean = modulator
        .modulate_symbols(&symbols, 0.0)
        .map_err(|e| anyhow::anyhow!("modulate: {e}"))?;
    clean.resize(WINDOW_SAMPLES, 0.0);

    let p_signal = signal_power(&clean);
    println!(
        "  reference signal '{}' at base freq, power = {:.6}",
        target_msg, p_signal
    );

    let cfg = Ft8Config::default();

    println!("\n  SNR (dB)  | σ          | success | rate");
    println!("  --------- | ---------- | ------- | -----");
    for &snr_db in &snrs {
        let sigma = sigma_for_snr_db(p_signal, snr_db);
        let mut success = 0;
        for r in 0..n_realizations {
            let mut rng = StdRng::seed_from_u64(7777 + r as u64);
            let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
            let recv: Vec<f32> = clean.iter().zip(&noise).map(|(s, n)| s + n).collect();
            let mut decoder = Ft8Decoder::new(cfg.clone())
                .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
            let decoded = decoder
                .decode_window(&recv)
                .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
            if decoded.iter().any(|d| d.text == target_msg) {
                success += 1;
            }
        }
        let rate = success as f64 / n_realizations as f64 * 100.0;
        println!(
            "    {:>+5.1}   | {:>10.6} | {:>4}/{:<3} | {:>4.1}%",
            snr_db, sigma, success, n_realizations, rate
        );
    }

    println!("\n### Notes");
    println!("  FT8 reference sensitivity: ~50% decode at -21 dB SNR (2500 Hz BW).");
    println!("  pancetta-ft8's curve above characterizes the production decoder's");
    println!("  synthetic-clean baseline. Compare to operator's real-world recall to");
    println!("  isolate corpus/channel effects from decoder limits.");

    Ok(())
}
