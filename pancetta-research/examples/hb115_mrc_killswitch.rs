//! hb-115 — Dual-KiwiSDR space-diversity LLR fusion (MRC) — kill-switch
//!
//! Mechanism-level feasibility check: does sample-domain MRC across two
//! synthetic independent-noise observations of the same WAV give
//! meaningful decode-rate improvement?
//!
//! Setup per bank entry hb-115 kill-switch:
//!   - Pick 5 hard-200 WAVs (the original recording is treated as the
//!     "signal").
//!   - For each WAV at multiple injected-noise levels σ², synthesize:
//!     `RX1 = WAV + n1` (n1 ∼ N(0, σ²)),
//!     `RX2 = WAV + n2` (n2 ∼ N(0, σ²), independent of n1),
//!     `MRC = (RX1 + RX2) / 2` ≈ WAV + (n1+n2)/2 → noise var σ²/2 → +3 dB.
//!   - Decode each (RX1, RX2, MRC).
//!   - PROCEED if MRC decode count exceeds max(RX1_count, RX2_count) by
//!     ≥ 30% on at least 3 / 5 WAVs at some σ² level.
//!
//! Note: this is the SAMPLE-DOMAIN MRC variant. LLR-domain fusion is the
//! eventual production target; sample-domain is equivalent for Gaussian
//! channels and is the cheap mechanism-validation step. If this passes,
//! the plan-sized next session writes a real LLR fusion path.
//!
//! Important framing: a synthetic-noise PROCEED proves only that the
//! mechanism (SNR adds → marginal decodes rescued) holds under
//! idealized independent noise. Real-world dual-KiwiSDR independence
//! is the LIVE-CAPTURE question, deferred to plan-sized follow-on.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb115_mrc_killswitch
//!
//! Tunable env vars:
//!   HB115_N_WAVS       (default 5)
//!   HB115_SEED         (default 1)
//!   HB115_NOISE_LEVELS (default "0.005,0.010,0.020,0.040" RMS amplitudes)
//!
//! Output: per-WAV table + verdict. No persistent artifact.

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn workspace_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_wav(path: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    anyhow::ensure!(
        spec.channels == 1 && spec.sample_rate == 12000,
        "expected 12 kHz mono"
    );
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    })
}

/// Box-Muller transform for N(0, sigma²) Gaussian samples.
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

#[derive(Debug, Clone)]
struct LevelResult {
    rx1: usize,
    rx2: usize,
    mrc: usize,
}

impl LevelResult {
    fn max_individual(&self) -> usize {
        self.rx1.max(self.rx2)
    }
    /// MRC lift relative to the better single RX. Returns 0.0 when both
    /// individuals are zero and MRC is also zero; otherwise (mrc - max) /
    /// max if max > 0, or `mrc as f64` (absolute) when max == 0 and mrc > 0.
    fn relative_lift(&self) -> f64 {
        let m = self.max_individual();
        if m == 0 {
            // When both individuals are zero, any MRC count is an
            // infinite-relative lift; report as a flag by returning
            // mrc count itself (caller treats > 0 as "saved 100%+").
            self.mrc as f64
        } else {
            (self.mrc as f64 - m as f64) / m as f64
        }
    }
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("no entries")?;

    let n_wavs: usize = std::env::var("HB115_N_WAVS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let seed: u64 = std::env::var("HB115_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let noise_levels: Vec<f32> = std::env::var("HB115_NOISE_LEVELS")
        .unwrap_or_else(|_| "0.005,0.010,0.020,0.040".to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    anyhow::ensure!(!noise_levels.is_empty(), "no noise levels");

    println!(
        "## hb-115 MRC kill-switch — {} WAVs, σ ∈ {:?}, seed={}",
        n_wavs, noise_levels, seed
    );

    let cfg = Ft8Config::default();

    // Per-σ accumulators for the verdict.
    let mut per_sigma_pass_count: Vec<usize> = vec![0; noise_levels.len()];
    let mut per_sigma_total: Vec<usize> = vec![0; noise_levels.len()];

    for entry in entries.iter().take(n_wavs) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let original = load_wav(Path::new(wav_path))?;
        let baseline = decode_count(&original, &cfg)?;

        println!(
            "\n=== {}  ({} samples, baseline={} decodes) ===",
            &sha[..8],
            original.len(),
            baseline
        );
        println!("    σ       rx1   rx2   mrc   max   lift");
        println!("    -----   ----  ----  ----  ----  --------");

        let mut rng = StdRng::seed_from_u64(
            seed.wrapping_add(u64::from_str_radix(&sha[..16], 16).unwrap_or(0)),
        );

        let mut levels = Vec::new();
        for (li, &sigma) in noise_levels.iter().enumerate() {
            let n1 = gaussian_noise(&mut rng, original.len(), sigma);
            let n2 = gaussian_noise(&mut rng, original.len(), sigma);
            let rx1: Vec<f32> = original.iter().zip(&n1).map(|(s, n)| s + n).collect();
            let rx2: Vec<f32> = original.iter().zip(&n2).map(|(s, n)| s + n).collect();
            let mrc: Vec<f32> = rx1.iter().zip(&rx2).map(|(a, b)| (a + b) * 0.5).collect();
            let r1 = decode_count(&rx1, &cfg)?;
            let r2 = decode_count(&rx2, &cfg)?;
            let rm = decode_count(&mrc, &cfg)?;
            let lr = LevelResult {
                rx1: r1,
                rx2: r2,
                mrc: rm,
            };
            let lift = lr.relative_lift();
            let lift_disp = if lr.max_individual() == 0 {
                if rm == 0 {
                    "0".to_string()
                } else {
                    format!("rescue+{}", rm)
                }
            } else {
                format!("{:+.0}%", lift * 100.0)
            };
            println!(
                "    {:5.3}   {:4}  {:4}  {:4}  {:4}  {}",
                sigma,
                lr.rx1,
                lr.rx2,
                lr.mrc,
                lr.max_individual(),
                lift_disp
            );

            per_sigma_total[li] += 1;
            // PROCEED criterion: MRC ≥ +30% over max(individual) — count as pass.
            // For the rescue case (both individuals = 0), require MRC ≥ 1.
            let pass = if lr.max_individual() == 0 {
                rm >= 1
            } else {
                lift >= 0.30
            };
            if pass {
                per_sigma_pass_count[li] += 1;
            }

            levels.push(lr);
        }
    }

    // Verdict.
    println!(
        "\n## Verdict (per-σ pass count, threshold ≥ 3/{} WAVs)",
        n_wavs
    );
    let mut any_proceed = false;
    for (li, &sigma) in noise_levels.iter().enumerate() {
        let p = per_sigma_pass_count[li];
        let t = per_sigma_total[li];
        let proceed = p * 5 >= t * 3; // p/t ≥ 3/5
        let tag = if proceed { "PROCEED" } else { "shelve  " };
        println!("    σ={:5.3}: {:>2}/{:>2} pass  →  {}", sigma, p, t, tag);
        if proceed {
            any_proceed = true;
        }
    }

    println!();
    if any_proceed {
        println!(
            "## Verdict: PROCEED  (≥ 30% MRC lift on ≥ 3/{} WAVs at some σ)",
            n_wavs
        );
        println!(
            "    Next: plan-sized session for live paired-Kiwi capture + LLR-domain MRC wire."
        );
    } else {
        println!(
            "## Verdict: SHELVE   (no σ achieves ≥ 30% MRC lift on 3/{} WAVs)",
            n_wavs
        );
        println!("    Mechanism not strong enough on synthetic-noise control; investing in paired-Kiwi capture not justified.");
    }

    Ok(())
}
