//! Batch 44 — wild probes (Tier 1+4 combined).
//!
//! 1. Reverse decode — time-reverse the WAV, decode. FT8 Costas pattern
//!    is positionally specific (symbols 0..7, 36..43, 72..79); reversing
//!    moves trailing Costas to leading position.
//! 2. Frequency dithering — shift WAV in frequency by {-1.5, -0.75, 0,
//!    +0.75, +1.5} Hz, decode each, union TPs. Tests sub-bin sensitivity.
//! 3. Sub-Hz freq search — shift WAV by {-0.25, -0.5, 0, +0.5, +0.25} Hz,
//!    much finer than freq_osr=2's 3.125 Hz bin spacing.
//! 4. ldpc_iter sweep {300, 400, 500} — push beyond Batch 41's 200.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch44_wild

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: f64 = 12_000.0;

fn workspace_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_wav(path: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    anyhow::ensure!(spec.channels == 1 && spec.sample_rate == 12000);
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    })
}

fn load_truth(ws: &Path, sha: &str) -> HashSet<String> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return HashSet::new();
    };
    v["decodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
        .collect()
}

fn decode_default(samples: &[f32], cfg: &Ft8Config) -> Result<HashSet<String>> {
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(decoded.into_iter().map(|d| d.text).collect())
}

/// Frequency shift via real-valued cosine modulation.
fn freq_shift(samples: &[f32], delta_hz: f64) -> Vec<f32> {
    let omega = 2.0 * std::f64::consts::PI * delta_hz / SAMPLE_RATE;
    samples
        .iter()
        .enumerate()
        .map(|(n, x)| {
            let phase = omega * n as f64;
            (*x as f64 * 2.0 * phase.cos()) as f32
        })
        .collect()
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();

    let mut base = Ft8Config::default();
    base.max_decode_passes = 2;
    base.ldpc_iterations = 200;

    // Subset for cheap probes
    let probe_n: usize = std::env::var("BATCH44_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    eprintln!("Probing first {} WAVs", probe_n);

    // Baseline (production 15s WAV, default config)
    eprintln!("baseline…");
    let mut baseline_tps = 0usize;
    let mut baseline_total = 0usize;
    for entry in entries.iter().take(probe_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let decoded = decode_default(&samples, &base)?;
        baseline_total += decoded.len();
        for t in &decoded {
            if truth.contains(t) {
                baseline_tps += 1;
            }
        }
    }

    println!("## Batch 44 — wild probes");
    println!(
        "\nBaseline (mp=2, ldpc=200, n={}): {} decodes / {} TPs",
        probe_n, baseline_total, baseline_tps
    );

    // === Reverse decode ===
    println!("\n### Reverse decode");
    let mut rev_tps = 0usize;
    let mut rev_total = 0usize;
    for entry in entries.iter().take(probe_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let reversed: Vec<f32> = samples.into_iter().rev().collect();
        let truth = load_truth(&ws, sha);
        let decoded = decode_default(&reversed, &base)?;
        rev_total += decoded.len();
        for t in &decoded {
            if truth.contains(t) {
                rev_tps += 1;
            }
        }
    }
    println!(
        "  reverse: {} decodes / {} TPs (Δ {:+})",
        rev_total,
        rev_tps,
        rev_tps as i64 - baseline_tps as i64
    );

    // === Coarse freq dithering ===
    println!("\n### Coarse freq dithering: ±{{1.5, 0.75}} Hz");
    let coarse_shifts: Vec<f64> = vec![-1.5, -0.75, 0.75, 1.5];
    let mut coarse_union_tps = 0usize;
    let mut coarse_per_shift_tps = vec![0usize; coarse_shifts.len()];
    for entry in entries.iter().take(probe_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let mut union: HashSet<String> = HashSet::new();
        // include baseline (df=0) in union
        union.extend(decode_default(&samples, &base)?);
        for (i, &df) in coarse_shifts.iter().enumerate() {
            let shifted = freq_shift(&samples, df);
            let decoded = decode_default(&shifted, &base)?;
            for t in &decoded {
                if truth.contains(t) {
                    coarse_per_shift_tps[i] += 1;
                }
            }
            union.extend(decoded);
        }
        for t in &union {
            if truth.contains(t) {
                coarse_union_tps += 1;
            }
        }
    }
    println!("  per-shift TPs: {:?}", coarse_per_shift_tps);
    println!(
        "  union (incl baseline): {} TPs (Δ {:+} from baseline)",
        coarse_union_tps,
        coarse_union_tps as i64 - baseline_tps as i64
    );

    // === Sub-Hz freq dithering ===
    println!("\n### Sub-Hz freq dithering: ±{{0.5, 0.25}} Hz");
    let fine_shifts: Vec<f64> = vec![-0.5, -0.25, 0.25, 0.5];
    let mut fine_union_tps = 0usize;
    let mut fine_per_shift_tps = vec![0usize; fine_shifts.len()];
    for entry in entries.iter().take(probe_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let mut union: HashSet<String> = HashSet::new();
        union.extend(decode_default(&samples, &base)?);
        for (i, &df) in fine_shifts.iter().enumerate() {
            let shifted = freq_shift(&samples, df);
            let decoded = decode_default(&shifted, &base)?;
            for t in &decoded {
                if truth.contains(t) {
                    fine_per_shift_tps[i] += 1;
                }
            }
            union.extend(decoded);
        }
        for t in &union {
            if truth.contains(t) {
                fine_union_tps += 1;
            }
        }
    }
    println!("  per-shift TPs: {:?}", fine_per_shift_tps);
    println!(
        "  union (incl baseline): {} TPs (Δ {:+} from baseline)",
        fine_union_tps,
        fine_union_tps as i64 - baseline_tps as i64
    );

    // === ldpc_iter extended sweep ===
    println!("\n### ldpc_iter sweep {{300, 400, 500}}");
    for iters in [300, 400, 500] {
        let mut cfg = base.clone();
        cfg.ldpc_iterations = iters;
        eprintln!("  ldpc={}…", iters);
        let mut tps = 0usize;
        let mut total = 0usize;
        for entry in entries.iter().take(probe_n) {
            let wav_path = entry["wav_path"].as_str().context("wav_path")?;
            let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
            let samples = load_wav(Path::new(wav_path))?;
            let truth = load_truth(&ws, sha);
            let decoded = decode_default(&samples, &cfg)?;
            total += decoded.len();
            for t in &decoded {
                if truth.contains(t) {
                    tps += 1;
                }
            }
        }
        println!(
            "  ldpc={}: {} decodes / {} TPs (Δ {:+})",
            iters,
            total,
            tps,
            tps as i64 - baseline_tps as i64
        );
    }

    Ok(())
}
