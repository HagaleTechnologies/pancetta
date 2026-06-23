//! Batch 34 / Phases 2 + 3A + 3B + 3D + 3G + 3H — combined coverage
//! bucketing (post hb-217 fix).
//!
//! Single decode pass per WAV; bucket each truth multiple ways:
//!  * by `dt_s` (slot position: early/mid/late)
//!  * by `freq_hz` (low / mid / high band region)
//!  * by callsign-1 length
//!  * by neighbor density (count of truths within ±25 Hz / ±1 s)
//!  * hb-218 follow-on: of capture-locked misses, how many are STILL
//!    locked after hb-217 fix?
//!  * also count pancetta's NOVEL emissions (decodes not in jt9 truth)
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch34_combined_buckets

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
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

#[derive(Debug, Clone)]
struct TruthDecode {
    text: String,
    freq_hz: f64,
    dt_s: f64,
    snr_db: f64,
}

fn load_baseline(ws: &Path, sha: &str) -> Vec<TruthDecode> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return Vec::new();
    };
    let Some(arr) = v["decodes"].as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|d| {
            Some(TruthDecode {
                text: d["message"].as_str()?.to_string(),
                freq_hz: d["freq_hz"].as_f64()?,
                dt_s: d["dt_s"].as_f64()?,
                snr_db: d["snr_db"].as_f64().unwrap_or(0.0),
            })
        })
        .collect()
}

fn dt_bucket(dt: f64) -> &'static str {
    if dt < 0.0 {
        "<0.0 (early/late)"
    } else if dt < 1.5 {
        "0.0..1.5"
    } else if dt < 2.5 {
        "1.5..2.5"
    } else if dt < 4.0 {
        "2.5..4.0"
    } else {
        ">=4.0"
    }
}

fn freq_bucket(f: f64) -> &'static str {
    if f < 500.0 {
        "<500"
    } else if f < 1000.0 {
        "500..1000"
    } else if f < 1500.0 {
        "1000..1500"
    } else if f < 2000.0 {
        "1500..2000"
    } else if f < 2500.0 {
        "2000..2500"
    } else {
        ">=2500"
    }
}

fn first_callsign(text: &str) -> Option<String> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut idx = 0;
    if tokens.first().copied() == Some("CQ") {
        idx = 1;
        if tokens.get(1).copied() == Some("DX") {
            idx = 2;
        }
    }
    let raw = *tokens.get(idx)?;
    let base = raw.split('/').next().unwrap_or(raw);
    if base.len() >= 3 && base.chars().any(|c| c.is_ascii_digit()) {
        Some(base.to_string())
    } else {
        None
    }
}

fn neighbor_density(truth: &[TruthDecode], target: &TruthDecode) -> usize {
    truth
        .iter()
        .filter(|t| {
            t.text != target.text
                && (t.freq_hz - target.freq_hz).abs() <= 25.0
                && (t.dt_s - target.dt_s).abs() <= 1.0
        })
        .count()
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH34_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    println!("## Batch 34 — combined coverage bucket diagnostics (post hb-217 fix)");

    let cfg = Ft8Config::default();
    let mut dt_buckets: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut freq_buckets: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut call_len_buckets: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    let mut neighbor_buckets: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    let mut capture_locked_strong = 0usize;
    let mut capture_locked_recovered_strong = 0usize;
    let mut total_pancetta_decodes = 0usize;
    let mut total_pancetta_novel = 0usize;

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_baseline(&ws, sha);
        let truth_set: HashSet<String> = truth.iter().map(|t| t.text.clone()).collect();

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        total_pancetta_decodes += decoded.len();
        for d in &decoded {
            if !truth_set.contains(&d.text) {
                total_pancetta_novel += 1;
            }
        }
        let recovered: HashSet<String> = decoded.into_iter().map(|d| d.text).collect();

        for t in &truth {
            let recovered_this = recovered.contains(&t.text);

            // dt
            let row = dt_buckets.entry(dt_bucket(t.dt_s)).or_insert((0, 0));
            row.0 += 1;
            if recovered_this {
                row.1 += 1;
            }

            // freq
            let row = freq_buckets.entry(freq_bucket(t.freq_hz)).or_insert((0, 0));
            row.0 += 1;
            if recovered_this {
                row.1 += 1;
            }

            // call-1 length
            if let Some(c) = first_callsign(&t.text) {
                let row = call_len_buckets.entry(c.len()).or_insert((0, 0));
                row.0 += 1;
                if recovered_this {
                    row.1 += 1;
                }
            }

            // neighbor density
            let n = neighbor_density(&truth, t);
            let row = neighbor_buckets.entry(n).or_insert((0, 0));
            row.0 += 1;
            if recovered_this {
                row.1 += 1;
            }

            // hb-218: capture-locked at strong SNR
            if t.snr_db >= -10.0 && n >= 1 {
                capture_locked_strong += 1;
                if recovered_this {
                    capture_locked_recovered_strong += 1;
                }
            }
        }
    }

    let summarize = |label: &str, m: &BTreeMap<&str, (usize, usize)>| {
        println!("\n### {} (truths / recovered / recall%)", label);
        let mut items: Vec<_> = m.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        for (b, (t, r)) in items {
            let pct = *r as f64 / (*t).max(1) as f64 * 100.0;
            println!("  {:<22} {:>6} {:>10} {:>5.1}%", b, t, r, pct);
        }
    };

    summarize("Phase 3A: dt-bucket recall", &dt_buckets);
    summarize("Phase 3B: freq-bucket recall", &freq_buckets);

    println!("\n### Phase 3D: callsign-1 length recall");
    println!(
        "  {:>6} {:>6} {:>10} {:>5}",
        "len", "truth", "recovered", "%"
    );
    for (&len, (t, r)) in &call_len_buckets {
        let pct = *r as f64 / (*t).max(1) as f64 * 100.0;
        println!("  {:>6} {:>6} {:>10} {:>5.1}%", len, t, r, pct);
    }

    println!("\n### Phase 3G: neighbor-density (truths within ±25 Hz / ±1 s)");
    println!("  {:>6} {:>6} {:>10} {:>5}", "n", "truth", "recovered", "%");
    for (&n, (t, r)) in &neighbor_buckets {
        let pct = *r as f64 / (*t).max(1) as f64 * 100.0;
        println!("  {:>6} {:>6} {:>10} {:>5.1}%", n, t, r, pct);
    }

    println!("\n### Phase 2 (hb-218): capture-locked strong-SNR truths");
    println!("  capture-locked strong truths:  {}", capture_locked_strong);
    println!(
        "  recovered:                     {} ({:.1}%)",
        capture_locked_recovered_strong,
        capture_locked_recovered_strong as f64 / capture_locked_strong.max(1) as f64 * 100.0
    );

    println!("\n### Phase 3H: pancetta emissions vs novel");
    println!("  total pancetta decodes: {}", total_pancetta_decodes);
    println!("  novel (not in jt9):     {}", total_pancetta_novel);
    println!(
        "  novel rate:             {:.1}%",
        total_pancetta_novel as f64 / total_pancetta_decodes.max(1) as f64 * 100.0
    );

    Ok(())
}
