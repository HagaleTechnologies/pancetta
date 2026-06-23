//! Batch 42 — broad probe sweep across the Tier 1+2 backlog.
//!
//! Variants:
//! - residual_min_sync_score sweep {1.0, 1.5, 2.0, 2.5} — HIGHEST PRIORITY
//! - min_sync_score sweep {2.0, 2.5} (vs 3.0 default)
//! - time_range sweep {1.5, 2.5, 3.0}
//! - max_sync_candidates {450, 500, 700}
//! - NMS enable
//! - adaptive_ldpc_iters enable
//! - HPF <300 Hz pre-decode
//! - DC offset removal pre-decode (Batch 41 +4 TP replication)
//! - Combined best-of stacked
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch42_probes

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

fn load_truths(ws: &Path, sha: &str) -> HashSet<String> {
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

fn run_pass(
    entries: &[Value],
    cfg: &Ft8Config,
    sample_xform: impl Fn(&mut [f32]),
) -> Result<(usize, usize)> {
    let ws = workspace_root()?;
    let mut total_decodes = 0usize;
    let mut total_tps = 0usize;
    for entry in entries.iter() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let mut samples = load_wav(Path::new(wav_path))?;
        sample_xform(&mut samples);
        let truth = load_truths(&ws, sha);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        total_decodes += decoded.len();
        for d in &decoded {
            if truth.contains(&d.text) {
                total_tps += 1;
            }
        }
    }
    Ok((total_decodes, total_tps))
}

/// HPF: 1st-order IIR (gentle slope) cutoff ~300 Hz at 12 kHz SR.
fn hpf_300hz(s: &mut [f32]) {
    // y[n] = α * (y[n-1] + x[n] - x[n-1]); α tuned for ~300 Hz cutoff at 12 kHz
    let alpha = 0.95f32;
    let mut prev_x = 0.0f32;
    let mut prev_y = 0.0f32;
    for x in s.iter_mut() {
        let y = alpha * (prev_y + *x - prev_x);
        prev_x = *x;
        prev_y = y;
        *x = y;
    }
}

fn dc_remove(s: &mut [f32]) {
    let mean = s.iter().copied().sum::<f32>() / s.len() as f32;
    for x in s.iter_mut() {
        *x -= mean;
    }
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

    // Batch 41 production baseline: Fast tier mp=2, ldpc=200
    let mut base = Ft8Config::default();
    base.max_decode_passes = 2;
    base.ldpc_iterations = 200;
    eprintln!("baseline (Fast-tier post-Batch-41)…");
    let (b_total, b_tps) = run_pass(&entries, &base, |_| {})?;
    println!("## Batch 42 — broad probe sweep");
    println!(
        "\n### Baseline (mp=2, ldpc=200): {} decodes / {} TPs",
        b_total, b_tps
    );

    let report = |label: &str, total: usize, tps: usize| {
        let dt = tps as i64 - b_tps as i64;
        let dd = total as i64 - b_total as i64;
        let prec = tps as f64 / total.max(1) as f64 * 100.0;
        println!(
            "  {:<48} {:>5} dec ({:+5}) / {:>5} TPs ({:+4}) / {:>5.1}% prec",
            label, total, dd, tps, dt, prec
        );
    };

    // === Tier 1 — residual_min_sync_score (HIGHEST PRIORITY) ===
    println!("\n### Tier 1 — residual_min_sync_score sweep (WSJT-X cascades 2.1→1.3)");
    for v in [1.0f64, 1.5, 2.0, 2.5] {
        eprintln!("  residual_min={}…", v);
        let mut cfg = base.clone();
        cfg.residual_min_sync_score = Some(v);
        let (t, p) = run_pass(&entries, &cfg, |_| {})?;
        report(&format!("residual_min_sync_score=Some({})", v), t, p);
    }

    // === Tier 1 — min_sync_score sweep (between current 3.0 and Batch 40 failed 1.0) ===
    println!("\n### Tier 1 — min_sync_score sweep");
    for v in [2.0f64, 2.5] {
        eprintln!("  min_sync_score={}…", v);
        let mut cfg = base.clone();
        cfg.min_sync_score = v;
        let (t, p) = run_pass(&entries, &cfg, |_| {})?;
        report(&format!("min_sync_score={}", v), t, p);
    }

    // === Tier 2 — time_range sweep ===
    println!("\n### Tier 2 — time_range sweep");
    for v in [1.5f64, 2.5, 3.0] {
        eprintln!("  time_range={}…", v);
        let mut cfg = base.clone();
        cfg.time_range = v;
        let (t, p) = run_pass(&entries, &cfg, |_| {})?;
        report(&format!("time_range={}", v), t, p);
    }

    // === Tier 2 — max_sync_candidates sweep ===
    println!("\n### Tier 2 — max_sync_candidates sweep");
    for v in [450usize, 500, 700] {
        eprintln!("  max_sync_candidates={}…", v);
        let mut cfg = base.clone();
        cfg.max_sync_candidates = v;
        let (t, p) = run_pass(&entries, &cfg, |_| {})?;
        report(&format!("max_sync_candidates={}", v), t, p);
    }

    // === Tier 2 — NMS enable ===
    println!("\n### Tier 2 — NMS enable");
    eprintln!("  nms_enabled=true…");
    let mut cfg = base.clone();
    cfg.nms_enabled = true;
    let (t, p) = run_pass(&entries, &cfg, |_| {})?;
    report("nms_enabled=true", t, p);

    // === Tier 2 — adaptive_ldpc_iters ===
    println!("\n### Tier 2 — adaptive_ldpc_iters");
    eprintln!("  adaptive_ldpc_iters=true…");
    let mut cfg = base.clone();
    cfg.adaptive_ldpc_iters = true;
    let (t, p) = run_pass(&entries, &cfg, |_| {})?;
    report("adaptive_ldpc_iters=true", t, p);

    // === Tier 2 — HPF <300 Hz pre-decode ===
    println!("\n### Tier 2 — HPF <300 Hz pre-decode");
    eprintln!("  hpf_300…");
    let (t, p) = run_pass(&entries, &base, hpf_300hz)?;
    report("hpf_300hz", t, p);

    // === DC offset removal replication ===
    println!("\n### DC offset removal (Batch 41 +4 TP replication)");
    eprintln!("  dc_remove…");
    let (t, p) = run_pass(&entries, &base, dc_remove)?;
    report("dc_remove", t, p);

    // === Combined best-of stacked (placeholder — pick winners below) ===
    println!("\n### Combined: residual_min=1.5 + dc_remove");
    eprintln!("  combo…");
    let mut cfg = base.clone();
    cfg.residual_min_sync_score = Some(1.5);
    let (t, p) = run_pass(&entries, &cfg, dc_remove)?;
    report("residual_min=1.5 + dc_remove", t, p);

    Ok(())
}
