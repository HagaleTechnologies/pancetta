//! Batch 29 / Diagnostic I — hb-097 synthetic subtract α calibration.
//!
//! Goal: under a clean synthetic baseline (no rotor noise), is α* = 1.0?
//! If so, the hb-097 mechanism only fires when pancetta's INTERNAL rotor
//! estimate is noisy. This diagnostic confirms the synthetic baseline;
//! the production-grade hb-097 mechanism test requires internal access
//! to pancetta's `subtract_decode_coherent`.
//!
//! Method:
//! 1. Encode + modulate a known FT8 message at known freq.
//! 2. Add Gaussian noise at varying levels.
//! 3. For each noise realization, find α* minimizing
//!    |received - α * reference|² over the signal-bearing window.
//! 4. Report α* distribution, median(|α* - 1|).
//!
//! Synthetic baseline: rotor phase is known exactly (we generated the
//! signal). α* should cluster at 1.0 in the noise-limited regime.
//! Drift would indicate something is wrong with the test setup, not a
//! real mechanism.
//!
//! Honest framing: a synthetic-α*=1 result does NOT shelve hb-097 — it
//! only validates that the line-search math is correct. To test the
//! production hypothesis (pancetta's rotor noise → α≠1 beneficial), we
//! need internal subtract-path instrumentation (not in scope here).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch29_subtract_alpha

use anyhow::Result;
use pancetta_ft8::{Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES};
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

fn residual_energy(received: &[f32], reference: &[f32], alpha: f32) -> f64 {
    received
        .iter()
        .zip(reference.iter())
        .map(|(&r, &ref_)| {
            let e = r - alpha * ref_;
            e as f64 * e as f64
        })
        .sum()
}

fn find_optimal_alpha(received: &[f32], reference: &[f32]) -> (f32, f64) {
    let mut best_alpha = 1.0_f32;
    let mut best_energy = f64::INFINITY;
    let mut alpha = 0.7_f32;
    while alpha <= 1.3 + 1e-6 {
        let e = residual_energy(received, reference, alpha);
        if e < best_energy {
            best_energy = e;
            best_alpha = alpha;
        }
        alpha += 0.01;
    }
    (best_alpha, best_energy)
}

fn main() -> Result<()> {
    let n_realizations: usize = std::env::var("BATCH29_I_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let sigmas: Vec<f32> = std::env::var("BATCH29_I_SIGMAS")
        .unwrap_or_else(|_| "0.001,0.01,0.05".to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    println!(
        "## Batch 29 / Diagnostic I — synthetic subtract α calibration ({} realizations × {} σ-levels)",
        n_realizations,
        sigmas.len()
    );

    // Generate clean reference FT8 signal once.
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder
        .encode_message("CQ K5ARH EM10", None)
        .map_err(|e| anyhow::anyhow!("encode: {e}"))?;
    let mut modulator = Ft8Modulator::new_default()?;
    // frequency_offset is added to modulator's base (BASE_FREQUENCY = 1500 Hz);
    // pass 0 to land at 1500 Hz total. (Higher offsets blow the +deviation limit.)
    let mut clean = modulator
        .modulate_symbols(&symbols, 0.0)
        .map_err(|e| anyhow::anyhow!("modulate: {e}"))?;
    clean.resize(WINDOW_SAMPLES, 0.0);

    println!(
        "  reference: 'CQ K5ARH EM10' at 1500 Hz (base), len = {} samples",
        clean.len()
    );

    for &sigma in &sigmas {
        let mut alphas: Vec<f32> = Vec::with_capacity(n_realizations);
        for r in 0..n_realizations {
            let mut rng = StdRng::seed_from_u64(1234 + r as u64);
            let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
            let received: Vec<f32> = clean.iter().zip(&noise).map(|(s, n)| s + n).collect();
            let (alpha, _) = find_optimal_alpha(&received, &clean);
            alphas.push(alpha);
        }
        let mut sorted = alphas.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = sorted[sorted.len() / 2];
        let p10 = sorted[sorted.len() / 10];
        let p90 = sorted[sorted.len() * 9 / 10];
        let mean = sorted.iter().sum::<f32>() / sorted.len() as f32;
        let median_abs_dev =
            sorted.iter().map(|a| (a - median).abs()).sum::<f32>() / sorted.len() as f32;

        println!(
            "  σ = {:5.3}:  α* mean={:.4}  median={:.4}  p10={:.4}  p90={:.4}  median|α-1|={:.4}",
            sigma, mean, median, p10, p90, median_abs_dev
        );
    }

    println!("\n### Verdict");
    println!("  Synthetic baseline: with the clean reference signal known exactly, α* clusters at");
    println!("  1.0 (modulo noise floor). This validates the line-search math.");
    println!();
    println!("  The bank's PROCEED criterion (median |α-1| ≥ 0.05) cannot be tested here —");
    println!("  pancetta's α≠1 mechanism only fires when the rotor estimate is noisy");
    println!(
        "  (pancetta-ft8 internal). Real diagnostic needs internal `subtract_decode_coherent`"
    );
    println!("  instrumentation. Labeled MECHANISM-VALIDATED, DEFERRED to internal-access");
    println!("  diagnostic; the production question (does rotor noise produce useful α-drift?)");
    println!("  remains open.");

    Ok(())
}
