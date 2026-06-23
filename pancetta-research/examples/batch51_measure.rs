//! Batch 51 — hard-200 toggle measurement of the 4 new mechanisms
//! (all default-OFF; this probe flips each one ON to see hard-200 impact):
//!
//! 1. **hb-237 Session 2 cross-sequence A7 decoder consumer**
//!    (`cross_sequence_a7_enabled = true`). Note: coordinator does not yet
//!    invoke `try_cross_sequence_decodes` (Session 3 deferral), so this
//!    flag toggle is functionally a no-op on hard-200 — included for the
//!    default-OFF byte-identity confirmation.
//! 2. **WSJT-X Improved a8 sequenced-QSO-state AP**
//!    (`a8_qso_state_ap_enabled = true`). Same caveat: hard-200 has no
//!    live QSO state, so a8 doesn't fire either. Default-OFF guard.
//! 3. **ft8mon three-stage sync cascade**
//!    (`three_stage_sync_cascade_enabled = true`). Improves subtraction
//!    quality, expected impact in mp=2 residual-decode pass.
//! 4. **WSJT-X mainline npre2 OSD preprocessing**
//!    (`osd_npre2_preprocessing_enabled = true`). Warm-starts OSD at
//!    depth ≥3 — pancetta defaults to OSD depth 1, so unlikely to fire on
//!    most hard-200 entries; included for the default-OFF guarantee.
//! 5. **All four ON together** — interaction probe.
//!
//! Baseline: `Ft8Config::default()` with `max_decode_passes = 2` and
//! `ldpc_iterations = 200` (5301 TPs reference).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch51_measure

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
    println!("## Batch 51 — new mechanisms toggle measurement");

    let notes = ws.join("research/notes/2026-06-09-batch51-measurement.md");
    std::fs::write(
        &notes,
        "# Batch 51 — new mechanism toggle measurement\n\n\
         | Config | Decodes | TPs | Δ vs baseline | Precision | Elapsed |\n\
         |---|---:|---:|---:|---:|---:|\n",
    )?;

    let append = |label: &str, total: usize, tps: usize, delta: i64, secs: f64| -> Result<()> {
        let precision = tps as f64 / total.max(1) as f64;
        let row =
            format!("| {label} | {total} | {tps} | {delta:+} | {precision:.4} | {secs:.1}s |\n");
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&notes)?;
        f.write_all(row.as_bytes())?;
        Ok(())
    };

    let cfg_base = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        ..Ft8Config::default()
    };
    eprintln!("baseline (all OFF)…");
    let (tot, tps_b, s) = run(&entries, &cfg_base)?;
    println!("\nbaseline: {tot} decodes / {tps_b} TPs ({s:.1}s)");
    append("baseline (all Batch 51 OFF)", tot, tps_b, 0, s)?;

    // 1. hb-237 Session 2
    let cfg = Ft8Config {
        cross_sequence_a7_enabled: true,
        ..cfg_base.clone()
    };
    eprintln!("hb-237 Session 2 (cross-sequence A7) ON…");
    let (t, p, s) = run(&entries, &cfg)?;
    let d = p as i64 - tps_b as i64;
    println!("hb-237 Session 2 ON: {t} / {p} TPs (Δ {d:+})");
    append("hb-237 Session 2 ON", t, p, d, s)?;

    // 2. a8
    let cfg = Ft8Config {
        a8_qso_state_ap_enabled: true,
        ..cfg_base.clone()
    };
    eprintln!("a8 QSO-state AP ON…");
    let (t, p, s) = run(&entries, &cfg)?;
    let d = p as i64 - tps_b as i64;
    println!("a8 ON: {t} / {p} TPs (Δ {d:+})");
    append("a8 QSO-state AP ON", t, p, d, s)?;

    // 3. three-stage sync
    let cfg = Ft8Config {
        three_stage_sync_cascade_enabled: true,
        ..cfg_base.clone()
    };
    eprintln!("three-stage sync cascade ON…");
    let (t, p, s) = run(&entries, &cfg)?;
    let d = p as i64 - tps_b as i64;
    println!("three-stage sync ON: {t} / {p} TPs (Δ {d:+})");
    append("three-stage sync cascade ON", t, p, d, s)?;

    // 4. npre2
    let cfg = Ft8Config {
        osd_npre2_preprocessing_enabled: true,
        ..cfg_base.clone()
    };
    eprintln!("npre2 OSD preprocessing ON…");
    let (t, p, s) = run(&entries, &cfg)?;
    let d = p as i64 - tps_b as i64;
    println!("npre2 ON: {t} / {p} TPs (Δ {d:+})");
    append("npre2 OSD preprocessing ON", t, p, d, s)?;

    // 5. All four
    let cfg = Ft8Config {
        cross_sequence_a7_enabled: true,
        a8_qso_state_ap_enabled: true,
        three_stage_sync_cascade_enabled: true,
        osd_npre2_preprocessing_enabled: true,
        ..cfg_base.clone()
    };
    eprintln!("all four ON…");
    let (t, p, s) = run(&entries, &cfg)?;
    let d = p as i64 - tps_b as i64;
    println!("all four ON: {t} / {p} TPs (Δ {d:+})");
    append("all four ON", t, p, d, s)?;

    println!("\nDone. Results in {}", notes.display());
    Ok(())
}
