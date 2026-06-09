//! Batch 50 — hard-200 toggle measurement of the 4 new Batch 50 mechanisms
//! (all default-OFF; this probe flips each one ON in turn to see its impact):
//!
//! 1. **LLR whitening** (`llr_whitening_enabled = true`)
//! 2. **Per-candidate frequency tracker** (`per_candidate_freq_tracker_enabled = true`)
//! 3. **WSJT-X Improved 4th-pass-after-a7** (`fourth_pass_after_a7_enabled = true`)
//! 4. **All four ON together** — interaction probe
//!
//! Note: **hb-237 cross-sequence A7** is NOT in this matrix — its decoder
//! consumption is Session 2 (deferred), so toggling the flag is a no-op
//! for recall (cache populates but no consumer reads it). Re-measure when
//! Session 2 ships.
//!
//! Baseline: `Ft8Config::default()` with `max_decode_passes = 2` and
//! `ldpc_iterations = 200` (matches batch48/49 5301-TP baseline).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch50_new_measure

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
    for entry in entries.iter() {
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
    }
    Ok((total, tps, t0.elapsed().as_secs_f64()))
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

    println!("loaded hard-200 manifest: {} entries", entries.len());
    println!("## Batch 50 — new mechanisms toggle measurement");
    println!("(mp=2, ldpc=200; each new flag ON individually then all-on)");

    let notes = ws.join("research/notes/2026-06-09-batch50-new-measurement.md");
    std::fs::write(&notes, "# Batch 50 — new mechanism toggle measurement\n\n| Config | Decodes | TPs | Δ vs baseline | Precision | Elapsed |\n|---|---:|---:|---:|---:|---:|\n")?;

    let append =
        |path: &Path, label: &str, total: usize, tps: usize, delta: i64, secs: f64| -> Result<()> {
            let precision = tps as f64 / total.max(1) as f64;
            let row = format!(
                "| {label} | {total} | {tps} | {delta:+} | {precision:.4} | {secs:.1}s |\n"
            );
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
            f.write_all(row.as_bytes())?;
            Ok(())
        };

    // Baseline
    let cfg_base = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        ..Ft8Config::default()
    };
    eprintln!("baseline (all OFF)…");
    let (tot, tps_b, s) = run(&entries, &cfg_base)?;
    println!("\nbaseline: {tot} decodes / {tps_b} TPs ({s:.1}s)");
    append(&notes, "baseline (all Batch 50 new OFF)", tot, tps_b, 0, s)?;

    // 1. LLR whitening ON
    let cfg_lw = Ft8Config {
        llr_whitening_enabled: true,
        ..cfg_base.clone()
    };
    eprintln!("LLR whitening ON…");
    let (t, p, s) = run(&entries, &cfg_lw)?;
    println!(
        "LLR whitening ON: {t} decodes / {p} TPs (Δ {:+})",
        p as i64 - tps_b as i64
    );
    append(&notes, "LLR whitening ON", t, p, p as i64 - tps_b as i64, s)?;

    // 2. Per-candidate freq tracker ON
    let cfg_ft = Ft8Config {
        per_candidate_freq_tracker_enabled: true,
        ..cfg_base.clone()
    };
    eprintln!("per-candidate freq tracker ON…");
    let (t, p, s) = run(&entries, &cfg_ft)?;
    println!(
        "freq tracker ON: {t} decodes / {p} TPs (Δ {:+})",
        p as i64 - tps_b as i64
    );
    append(
        &notes,
        "per-candidate freq tracker ON",
        t,
        p,
        p as i64 - tps_b as i64,
        s,
    )?;

    // 3. 4th-pass-after-a7 ON
    let cfg_4p = Ft8Config {
        fourth_pass_after_a7_enabled: true,
        ..cfg_base.clone()
    };
    eprintln!("4th-pass-after-a7 ON…");
    let (t, p, s) = run(&entries, &cfg_4p)?;
    println!(
        "4th-pass-after-a7 ON: {t} decodes / {p} TPs (Δ {:+})",
        p as i64 - tps_b as i64
    );
    append(
        &notes,
        "4th-pass-after-a7 ON",
        t,
        p,
        p as i64 - tps_b as i64,
        s,
    )?;

    // 4. All three on
    let cfg_all = Ft8Config {
        llr_whitening_enabled: true,
        per_candidate_freq_tracker_enabled: true,
        fourth_pass_after_a7_enabled: true,
        ..cfg_base.clone()
    };
    eprintln!("all three ON…");
    let (t, p, s) = run(&entries, &cfg_all)?;
    println!(
        "all three ON: {t} decodes / {p} TPs (Δ {:+})",
        p as i64 - tps_b as i64
    );
    append(&notes, "all three new ON", t, p, p as i64 - tps_b as i64, s)?;

    println!("\nDone. Results in {}", notes.display());
    Ok(())
}
