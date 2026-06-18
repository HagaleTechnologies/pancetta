//! hb-228 — JTDX 3-method spectral sweep probe.
//!
//! Measures `three_method_spectral_sweep_enabled` (sqrt + linear + power Costas
//! sync candidate union on pass 0) against the production baseline
//! (`Ft8Config::default()`) on a corpus tier with ft8_lib truth.
//!
//! Per probe-baseline discipline, defaults to a fast N=50 cap before any full
//! run. Override the cap with `HB228_N=<n>` (or `HB228_N=0` for the whole
//! manifest). Delta (baseline vs hb-228) is the decision signal — raw exact-text
//! truth matching affects BOTH configs equally, so the Δ is valid even though
//! absolute precision is understated by hashed-callsign aliasing.
//!
//! Decision rule: ΔTPs > 0 AND ΔFPs ≤ 2·ΔTPs → graduation candidate (full run);
//! ΔTPs ≤ 0 → shelve.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb228_three_method_sweep

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
    for entry in entries.iter() {
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
    }
    eprintln!(
        "  [{label}] {} entries, {tps} TPs / {tot} dec ({:.0}s)",
        entries.len(),
        t0.elapsed().as_secs_f64()
    );
    Ok((tot, tps, t0.elapsed().as_secs_f64()))
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/raw_530_full.manifest.json"),
    )?)?;
    let mut entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();

    let n: usize = std::env::var("HB228_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    if n > 0 && entries.len() > n {
        entries.truncate(n);
    }
    println!("hb-228 probe: {} entries (HB228_N={n})", entries.len());

    let base_cfg = Ft8Config::default();
    let configs: Vec<(&str, Ft8Config)> = vec![
        ("baseline", base_cfg.clone()),
        (
            "hb-228 three_method_spectral_sweep",
            Ft8Config {
                three_method_spectral_sweep_enabled: true,
                ..base_cfg.clone()
            },
        ),
    ];

    let mut rows: Vec<(String, usize, usize, f64, f64)> = Vec::new();
    for (label, cfg) in &configs {
        eprintln!("---- {label} ----");
        let (tot, tps, secs) = run(label, &entries, cfg)?;
        let prec = tps as f64 / tot.max(1) as f64;
        println!("{label}: {tot} dec / {tps} TPs / prec {prec:.4} / {secs:.0}s");
        rows.push((label.to_string(), tot, tps, prec, secs));
    }

    let (base_tot, base_tps, base_prec) = (rows[0].1, rows[0].2, rows[0].3);
    println!("\n## hb-228 3-method spectral sweep on raw_530_full[..{n}] (ft8_lib truth)\n");
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
    Ok(())
}
