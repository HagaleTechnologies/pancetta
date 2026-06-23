//! Batch 53 — LLR whitening validation on hard_1000.
//!
//! Batch 50 measured `llr_whitening_enabled = true` on hard-200:
//!   - +2 TPs (5301 → 5303)
//!   - precision 0.7487 → 0.7689 (+2.7%)
//! Best individual mechanism in the Batch 50 wave; strongest candidate
//! to flip default-ON. This probe re-measures on hard_1000 (5×) to see
//! whether the lift survives at scale.
//!
//! Two configs at `max_decode_passes = 2, ldpc_iterations = 200`:
//!   1. baseline (llr_whitening OFF)
//!   2. llr_whitening ON
//!
//! Decision rule:
//!   - If Δ TPs > 0 AND precision improves → recommend default-ON for Batch 54.
//!   - If Δ TPs ≤ 0 OR precision regresses → keep default-OFF; the hard-200 lift was noise.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch53_llr_whitening_hard1000

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashSet;
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

fn run(entries: &[Value], cfg: &Ft8Config) -> Result<(usize, usize, f64)> {
    let ws = workspace_root()?;
    let mut total = 0usize;
    let mut tps = 0usize;
    let t0 = std::time::Instant::now();
    for (i, entry) in entries.iter().enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        total += decoded.len();
        for d in &decoded {
            if truth.contains(&d.text) {
                tps += 1;
            }
        }
        if (i + 1) % 100 == 0 {
            eprintln!(
                "    [{}/{}] running… (tps so far: {tps})",
                i + 1,
                entries.len()
            );
        }
    }
    Ok((total, tps, t0.elapsed().as_secs_f64()))
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_1000.manifest.json"),
    )?)?;
    let entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();
    println!("loaded hard_1000: {} entries", entries.len());

    let cfg_off = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        llr_whitening_enabled: false,
        ..Ft8Config::default()
    };
    let cfg_on = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        llr_whitening_enabled: true,
        ..Ft8Config::default()
    };

    eprintln!("baseline (llr_whitening OFF)…");
    let (tot_b, tps_b, secs_b) = run(&entries, &cfg_off)?;
    println!(
        "\nbaseline (whitening OFF): {tot_b} decodes / {tps_b} TPs ({secs_b:.1}s, prec {:.4})",
        tps_b as f64 / tot_b.max(1) as f64
    );

    eprintln!("llr_whitening ON…");
    let (tot_on, tps_on, secs_on) = run(&entries, &cfg_on)?;
    let delta = tps_on as i64 - tps_b as i64;
    println!(
        "whitening ON: {tot_on} decodes / {tps_on} TPs ({secs_on:.1}s, Δ {delta:+}, prec {:.4})",
        tps_on as f64 / tot_on.max(1) as f64
    );

    let notes_path = ws.join("research/notes/2026-06-09-batch53-llr-whitening-hard1000.md");
    let prec_b = tps_b as f64 / tot_b.max(1) as f64;
    let prec_on = tps_on as f64 / tot_on.max(1) as f64;
    let recommend = if delta > 0 && prec_on > prec_b {
        "**Recommend default-ON in Batch 54**: TPs ↑ AND precision ↑ at 5× corpus scale."
    } else if delta == 0 && prec_on > prec_b {
        "**Borderline**: TPs flat but precision lifts. Consider a hard-yet-bigger corpus before flipping."
    } else if delta > 0 && prec_on <= prec_b {
        "**Caution**: TPs lift but precision flat-or-regresses. The Batch 50 +2.7% precision lift did NOT survive scale-out — keep default-OFF."
    } else {
        "**Keep default-OFF**: hard-200 lift was noise (Δ TPs ≤ 0 or precision regressed at scale)."
    };
    let body = format!(
        "# Batch 53 — LLR whitening on hard_1000\n\n\
         Re-measurement of Batch 50's +2 TPs / +2.7% precision finding at 5× corpus scale.\n\n\
         Config: `max_decode_passes = 2, ldpc_iterations = 200`. Only `llr_whitening_enabled` toggled.\n\n\
         | Config | Decodes | TPs | Δ vs baseline | Precision | Elapsed |\n\
         |---|---:|---:|---:|---:|---:|\n\
         | baseline (whitening OFF) | {tot_b} | {tps_b} | 0 | {prec_b:.4} | {secs_b:.1}s |\n\
         | whitening ON | {tot_on} | {tps_on} | {delta:+} | {prec_on:.4} | {secs_on:.1}s |\n\n\
         {recommend}\n"
    );
    std::fs::write(&notes_path, body)?;
    println!("\nDone. Results in {}", notes_path.display());
    Ok(())
}
