//! Batch 92 — Costas half-loop plateau A/B (Batch 88 residual #1).
//!
//! `compute_costas_score_groups` historically takes `max` over
//! `half ∈ {0,1}`, but with TIME_OSR=2 the t0 sweep already visits
//! half-symbol offsets, so `score(t0) = max(g(t0), g(t0+1))` — a
//! two-step plateau whose tie-break emits ~8% of candidates one sync
//! step (960 samples) early (Batch 88 Part C −960 bucket). This example
//! decodes raw_530_full slots twice — `costas_half_loop_disabled =
//! false` (baseline) vs `true` (half=0 only) — and scores both against
//! ft8_lib truth with hash-normalized matching (Batch 87 rule).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch92_costas_half_loop
//!   cargo run --release -p pancetta-research --example batch92_costas_half_loop -- --full
//!
//! Default: first 200 slots. `--full`: all 2066 slots.

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use pancetta_research::metrics::hash_normalize_message;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
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

struct Slot {
    wav_path: String,
    truth_norm: HashSet<String>,
}

#[derive(Default)]
struct Tally {
    slots: usize,
    decodes: usize,
    tp: usize,
    fp: usize,
    truth_n: usize,
    found: usize,
}

fn run_config(slots: &[Slot], cfg: &Ft8Config, label: &str) -> Result<(Tally, f64)> {
    let start = Instant::now();
    let next = AtomicUsize::new(0);
    let tally = Mutex::new(Tally::default());
    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(slots.len().max(1));
    std::thread::scope(|s| -> Result<()> {
        let mut handles = Vec::new();
        for _ in 0..n_threads {
            handles.push(s.spawn(|| -> Result<()> {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= slots.len() {
                        return Ok(());
                    }
                    let slot = &slots[i];
                    let Ok(samples) = load_wav(Path::new(&slot.wav_path)) else {
                        continue;
                    };
                    let mut decoder = Ft8Decoder::new(cfg.clone())
                        .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
                    let decoded = decoder
                        .decode_window(&samples)
                        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
                    let dec_norm: HashSet<String> = decoded
                        .iter()
                        .map(|d| hash_normalize_message(&d.text))
                        .collect();
                    let mut t = tally.lock().unwrap();
                    t.slots += 1;
                    t.decodes += decoded.len();
                    for d in &dec_norm {
                        if slot.truth_norm.contains(d) {
                            t.tp += 1;
                        } else {
                            t.fp += 1;
                        }
                    }
                    t.truth_n += slot.truth_norm.len();
                    t.found += slot.truth_norm.intersection(&dec_norm).count();
                }
            }));
        }
        for h in handles {
            h.join().expect("worker panicked")?;
        }
        Ok(())
    })?;
    let wall = start.elapsed().as_secs_f64();
    let t = tally.into_inner().unwrap();
    println!(
        "[{label}] slots={} decodes={} TP={} FP={} truth={} found={} miss_rate={:.2}% wall={wall:.1}s",
        t.slots,
        t.decodes,
        t.tp,
        t.fp,
        t.truth_n,
        t.found,
        100.0 * (1.0 - t.found as f64 / t.truth_n.max(1) as f64)
    );
    Ok((t, wall))
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let full = std::env::args().any(|a| a == "--full");
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/raw_530_full.manifest.json"),
    )?)?;
    let take = if full { usize::MAX } else { 200 };
    let mut slots: Vec<Slot> = Vec::new();
    for entry in manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .take(take)
    {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let truth_path = ws
            .join("research/baselines/ft8")
            .join(format!("{sha}.ft8lib.json"));
        let Ok(txt) = std::fs::read_to_string(&truth_path) else {
            continue;
        };
        let v: Value = serde_json::from_str(&txt)?;
        let truth_norm: HashSet<String> = v["decodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["message"].as_str())
            .map(hash_normalize_message)
            .collect();
        slots.push(Slot {
            wav_path: wav_path.to_string(),
            truth_norm,
        });
    }
    println!(
        "corpus=raw_530_full slots_with_truth={} ({})",
        slots.len(),
        if full { "full" } else { "first 200" }
    );

    let cfg_off = Ft8Config::default();
    let cfg_on = Ft8Config {
        costas_half_loop_disabled: true,
        ..Default::default()
    };

    let (t_off, w_off) = run_config(&slots, &cfg_off, "half-loop ON (baseline, flag=false)")?;
    let (t_on, w_on) = run_config(&slots, &cfg_on, "half-loop OFF (flag=true)")?;

    println!(
        "delta: TP {:+} FP {:+} decodes {:+} found {:+} wall {:+.1}s",
        t_on.tp as i64 - t_off.tp as i64,
        t_on.fp as i64 - t_off.fp as i64,
        t_on.decodes as i64 - t_off.decodes as i64,
        t_on.found as i64 - t_off.found as i64,
        w_on - w_off
    );
    Ok(())
}
