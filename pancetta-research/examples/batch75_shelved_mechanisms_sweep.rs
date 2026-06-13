//! Batch 75 — mass re-test of 6 shelved mechanisms on raw_530_full.
//!
//! Applies the Batch 66 pipeline to the remaining shelved mechanisms
//! per user iteration plan step 2.
//!
//! Mechanisms tested (one config flag each, default-OFF):
//!   1. costas_partial_metric (hb-242)
//!   2. costas_two_baseline (wide-lag baseline)
//!   3. three_stage_sync_cascade
//!   4. fourth_pass_after_a7
//!   5. cycle_audio_smoothing (hb-230)
//!   6. gaussian_ramp_subtract (hb-226)
//!
//! Each is measured against the production baseline (current
//! Ft8Config::default()) on raw_530_full with ft8_lib truth.
//!
//! Wall-clock: ~6 hours for 7 configs × 2066 slots.
//!
//! Decision rule per mechanism:
//!   - Δ TPs ≥ +20 AND Δ FPs ≤ +50: graduation candidate
//!   - Δ TPs > 0 AND Δ FPs ≤ 0: precision-positive ship
//!   - Otherwise: confirm shelved
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch75_shelved_mechanisms_sweep

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
        .join(format!("{sha}.ft8lib.json"));
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

fn run(label: &str, entries: &[Value], cfg: &Ft8Config) -> Result<(usize, usize, f64)> {
    let ws = workspace_root()?;
    let mut tot = 0;
    let mut tps = 0;
    let t0 = std::time::Instant::now();
    for (i, entry) in entries.iter().enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = match load_wav(Path::new(wav_path)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let truth = load_truth(&ws, sha);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        tot += decoded.len();
        for d in &decoded {
            if truth.contains(&d.text) {
                tps += 1;
            }
        }
        if (i + 1) % 200 == 0 {
            eprintln!(
                "  [{label} {}/{}] tps={tps} ({:.0}s)",
                i + 1,
                entries.len(),
                t0.elapsed().as_secs_f64()
            );
        }
    }
    Ok((tot, tps, t0.elapsed().as_secs_f64()))
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/raw_530_full.manifest.json"),
    )?)?;
    let entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();
    println!("loaded raw_530_full: {} entries", entries.len());

    let base_cfg = Ft8Config::default();

    let mut configs: Vec<(&str, Ft8Config)> = Vec::new();
    configs.push(("baseline (all OFF)", base_cfg.clone()));
    configs.push((
        "hb-242 costas_partial_metric",
        Ft8Config {
            costas_partial_metric_enabled: true,
            ..base_cfg.clone()
        },
    ));
    configs.push((
        "wide-lag costas_two_baseline",
        Ft8Config {
            costas_two_baseline_enabled: true,
            ..base_cfg.clone()
        },
    ));
    configs.push((
        "three_stage_sync_cascade",
        Ft8Config {
            three_stage_sync_cascade_enabled: true,
            ..base_cfg.clone()
        },
    ));
    configs.push((
        "fourth_pass_after_a7",
        Ft8Config {
            fourth_pass_after_a7_enabled: true,
            ..base_cfg.clone()
        },
    ));
    configs.push((
        "hb-230 cycle_audio_smoothing",
        Ft8Config {
            cycle_audio_smoothing_enabled: true,
            ..base_cfg.clone()
        },
    ));
    configs.push((
        "hb-226 gaussian_ramp_subtract",
        Ft8Config {
            gaussian_ramp_subtract_enabled: true,
            ..base_cfg.clone()
        },
    ));

    let mut rows: Vec<(String, usize, usize, f64, f64)> = Vec::new();
    for (label, cfg) in &configs {
        eprintln!("---- {label} ----");
        let (tot, tps, secs) = run(label, &entries, cfg)?;
        let prec = tps as f64 / tot.max(1) as f64;
        println!("{label}: {tot} dec / {tps} TPs / prec {prec:.4} / {secs:.0}s");
        rows.push((label.to_string(), tot, tps, prec, secs));
    }

    let (base_tot, base_tps, base_prec) = (rows[0].1, rows[0].2, rows[0].3);

    println!("\n## Shelved-mechanism sweep on raw_530_full (vs current default baseline)\n");
    println!("| Label | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec | Wall |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for (label, tot, tps, prec, secs) in &rows {
        let fps = tot - tps;
        let base_fps = base_tot - base_tps;
        let dtps = *tps as i64 - base_tps as i64;
        let dfps = fps as i64 - base_fps as i64;
        let dprec = prec - base_prec;
        println!(
            "| {label} | {tot} | {tps} | {dtps:+} | {fps} | {dfps:+} | {prec:.4} | {dprec:+.4} | {secs:.0}s |"
        );
    }

    let notes_path = ws.join("research/notes/2026-06-09-batch75-shelved-mechanisms.md");
    let mut body = String::new();
    body.push_str("# Batch 75 — shelved-mechanism sweep on raw_530_full (ft8_lib truth)\n\n");
    body.push_str("| Label | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec |\n");
    body.push_str("|---|---:|---:|---:|---:|---:|---:|---:|\n");
    for (label, tot, tps, prec, _) in &rows {
        let fps = tot - tps;
        let base_fps = base_tot - base_tps;
        let dtps = *tps as i64 - base_tps as i64;
        let dfps = fps as i64 - base_fps as i64;
        let dprec = prec - base_prec;
        body.push_str(&format!(
            "| {label} | {tot} | {tps} | {dtps:+} | {fps} | {dfps:+} | {prec:.4} | {dprec:+.4} |\n"
        ));
    }
    std::fs::write(&notes_path, body)?;
    Ok(())
}
