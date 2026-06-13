//! Batch 49 — final stanza: combined (hb-242 ON + wide-lag ON) stacked.
//!
//! Split from `batch49_widelag_tuning` because the Bash 10-min timeout
//! kills wrappers; running the combined config standalone fits in budget.
//!
//! Uses best-from-prior-sweep: hb-242 max_sync=300 (the only budget that
//! didn't make hb-242 worse; still net -18 alone), wide-lag pct=0.30,
//! norm=1.20 (a tied-best wide-lag config).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch49_combined

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

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
    let start = Instant::now();
    let mut total = 0usize;
    let mut tps = 0usize;
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
    Ok((total, tps, start.elapsed().as_secs_f64()))
}

fn append_result(notes_path: &Path, line: &str) -> Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(notes_path)?;
    writeln!(f, "{}", line)?;
    Ok(())
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

    let notes_path = ws.join("research/notes/2026-06-08-batch49-tuning-results.md");
    let b_tps: usize = 5301;

    let mut cfg = Ft8Config::default();
    cfg.max_decode_passes = 2;
    cfg.ldpc_iterations = 200;
    cfg.max_sync_candidates = 300;
    cfg.costas_partial_metric_enabled = true; // hb-242 ON
    cfg.costas_two_baseline_enabled = true; // wide-lag ON
    cfg.costas_two_baseline_percentile = 0.30;
    cfg.costas_two_baseline_norm_threshold = 1.20;

    eprintln!(">>> combined (hb-242 + wide-lag stacked)…");
    let (decodes, tps, secs) = run(&entries, &cfg)?;
    let delta = tps as i64 - b_tps as i64;
    let precision = tps as f64 / decodes.max(1) as f64;
    let line = format!(
        "| combined (hb-242 ON max_sync=300 + wide-lag pct=0.30 norm=1.20) | {decodes} | {tps} | {delta:+} | {precision:.4} | {secs:.1}s |"
    );
    println!("{line}");
    append_result(&notes_path, &line)?;

    println!("\nDone. Result saved to {}", notes_path.display());
    Ok(())
}
