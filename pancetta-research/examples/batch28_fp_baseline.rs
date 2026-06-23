//! Batch 28 / Diagnostic D — FP-injection baseline (hb-132)
//!
//! Question: when the decoder is fed pure noise (no signal present),
//! how often does it emit a CRC-passing decode anyway? This is the
//! "background FP rate" — the lower bound that any FP-filter must
//! beat to be useful.
//!
//! Method:
//!   1. Generate N synthetic 12 kHz mono 15-s windows of pure Gaussian
//!      noise at varying RMS levels.
//!   2. Decode each with `Ft8Config::default()`.
//!   3. Count emitted decodes per window. Any emit on noise-only input
//!      is a structural FP.
//!
//! Per hb-132: PROCEED to FP-injection-AUC corpus construction if FP
//! rate > 0.05 per window (1 in 20 noise windows leaks an FP). SHELVE
//! if FP rate ≈ 0 (existing CRC + plausibility gates are already
//! handling random-noise FPs).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch28_fp_baseline

use anyhow::Result;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const SAMPLE_RATE: usize = 12_000;
const SLOT_S: usize = 15;
const WINDOW_SAMPLES: usize = SAMPLE_RATE * SLOT_S;

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

fn decode_count(samples: &[f32], cfg: &Ft8Config) -> Result<usize> {
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(decoded.len())
}

fn main() -> Result<()> {
    let n_per_level: usize = std::env::var("BATCH28_FP_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let seed: u64 = std::env::var("BATCH28_FP_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let sigmas: Vec<f32> = std::env::var("BATCH28_FP_SIGMAS")
        .unwrap_or_else(|_| "0.01,0.05,0.1,0.5".to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    anyhow::ensure!(!sigmas.is_empty());

    println!("## Batch 28 / Diagnostic D — FP-injection baseline (pure noise)");
    println!("  {} windows per σ; σ levels {:?}", n_per_level, sigmas);

    let cfg = Ft8Config::default();

    let mut all_fp_rate: f64 = 0.0;
    let mut all_total = 0usize;

    println!("\n  σ        windows  fp_windows  total_fps  fp_rate(per_window)");
    println!("  -----    -------  ----------  ---------  --------------------");
    for &sigma in &sigmas {
        let mut rng = StdRng::seed_from_u64(seed.wrapping_add((sigma * 1000.0) as u64));
        let mut fp_windows = 0usize;
        let mut total_fps = 0usize;
        for _ in 0..n_per_level {
            let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
            let n_decodes = decode_count(&noise, &cfg)?;
            if n_decodes > 0 {
                fp_windows += 1;
                total_fps += n_decodes;
            }
        }
        let rate = fp_windows as f64 / n_per_level as f64;
        all_fp_rate += rate;
        all_total += 1;
        println!(
            "  {:5.3}    {:>7}  {:>10}  {:>9}  {:>20.4}",
            sigma, n_per_level, fp_windows, total_fps, rate
        );
    }

    let mean_fp_rate = if all_total == 0 {
        0.0
    } else {
        all_fp_rate / all_total as f64
    };
    println!();
    println!(
        "  Mean FP rate across σ levels: {:.4} per window",
        mean_fp_rate
    );
    println!();
    if mean_fp_rate >= 0.05 {
        println!("## Verdict: PROCEED — pure-noise FP rate ≥ 0.05/window. FP filter has surface area; build FP-injection-AUC corpus to characterize.");
    } else if mean_fp_rate >= 0.005 {
        println!("## Verdict: WEAK SHELVE — FP rate is positive but rare ({:.4}/window). Marginally exploitable, low priority.", mean_fp_rate);
    } else {
        println!("## Verdict: SHELVE — pure-noise FP rate ≈ 0. Existing CRC + plausibility gates already handle random-noise FPs; FP-injection AUC has no leverage.");
    }

    Ok(())
}
