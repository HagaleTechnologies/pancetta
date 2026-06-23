//! Batch 37 / Items BB1 + BB2 + CC1 — follow-on analysis on the AA frontier.
//!
//! BB1: Δdt distribution among frontier (time-domain separation potential)
//! BB2: WAV concentration of the 974 frontier entries
//! CC1: was the strong-neighbor decoded by pancetta? Re-runs mp=2 to check.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch37_frontier_bb

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
struct FrontierEntry {
    sha: String,
    text: String,
    freq_hz: f64,
    dt_s: f64,
    snr_db: f64,
    neighbor_text: String,
    neighbor_freq_hz: f64,
    neighbor_dt_s: f64,
    neighbor_snr_db: f64,
    delta_freq_hz: f64,
    delta_snr_db: f64,
}

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

fn ddt_bucket(ddt: f64) -> &'static str {
    let a = ddt.abs();
    if a < 0.1 {
        "0-0.1s"
    } else if a < 0.3 {
        "0.1-0.3s"
    } else if a < 0.5 {
        "0.3-0.5s"
    } else if a < 1.0 {
        "0.5-1.0s"
    } else {
        "1.0-1.5s"
    }
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let frontier_path = ws.join("research/scorecards/batch37_frontier.json");
    let frontier_json = std::fs::read_to_string(&frontier_path)?;
    let frontier: Vec<FrontierEntry> = serde_json::from_str(&frontier_json)?;
    println!(
        "## Batch 37 — BB1 + BB2 + CC1 (frontier n={})",
        frontier.len()
    );

    // BB1 — Δdt distribution
    println!("\n### BB1 — Δdt distribution (truth - neighbor, abs)");
    let mut ddt_b: BTreeMap<&'static str, usize> = BTreeMap::new();
    for f in &frontier {
        let ddt = f.dt_s - f.neighbor_dt_s;
        *ddt_b.entry(ddt_bucket(ddt)).or_insert(0) += 1;
    }
    println!("  {:<10} {:>6}", "Δdt", "count");
    for (k, v) in &ddt_b {
        println!("  {:<10} {:>6}", k, v);
    }

    // BB2 — WAV concentration
    println!("\n### BB2 — WAV concentration");
    let mut per_wav: BTreeMap<String, usize> = BTreeMap::new();
    for f in &frontier {
        *per_wav.entry(f.sha.clone()).or_insert(0) += 1;
    }
    let total_wavs = per_wav.len();
    let mut dist: BTreeMap<&'static str, usize> = BTreeMap::new();
    for &n in per_wav.values() {
        let b = match n {
            1 => "1 frontier",
            2..=3 => "2-3 frontier",
            4..=6 => "4-6 frontier",
            7..=10 => "7-10 frontier",
            _ => "11+ frontier",
        };
        *dist.entry(b).or_insert(0) += 1;
    }
    println!("  WAVs holding frontier entries: {}", total_wavs);
    println!("  {:<15} {:>6}", "bucket", "WAVs");
    for (k, v) in &dist {
        println!("  {:<15} {:>6}", k, v);
    }
    let max = per_wav.values().max().copied().unwrap_or(0);
    let total = per_wav.values().sum::<usize>();
    println!(
        "  max frontier entries in single WAV: {} (mean {:.1})",
        max,
        total as f64 / total_wavs.max(1) as f64
    );

    // CC1 — was the strong-neighbor decoded by pancetta (mp=2)?
    println!("\n### CC1 — was the neighbor truth decoded by pancetta mp=2?");
    println!("(re-running mp=2 on hard-200, this takes a few minutes)");

    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH37_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    let mut cfg = Ft8Config::default();
    cfg.max_decode_passes = 2;
    let mut decoded_per_wav: HashMap<String, HashSet<String>> = HashMap::new();
    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        let set: HashSet<String> = decoded.into_iter().map(|d| d.text).collect();
        decoded_per_wav.insert(sha.to_string(), set);
    }

    let mut neighbor_decoded = 0usize;
    let mut neighbor_missed = 0usize;
    let mut neighbor_decoded_missed_louder = 0usize;
    let mut neighbor_decoded_missed_quieter = 0usize;
    let mut neighbor_missed_missed_louder = 0usize;
    let mut neighbor_missed_missed_quieter = 0usize;
    for f in &frontier {
        let was_decoded = decoded_per_wav
            .get(&f.sha)
            .map(|s| s.contains(&f.neighbor_text))
            .unwrap_or(false);
        // Δsnr > 0 means neighbor is louder (truth is quieter)
        // Δsnr < 0 means neighbor is quieter (truth is louder — the "missed_louder" bucket)
        let truth_louder = f.delta_snr_db < -3.0;
        if was_decoded {
            neighbor_decoded += 1;
            if truth_louder {
                neighbor_decoded_missed_louder += 1;
            } else {
                neighbor_decoded_missed_quieter += 1;
            }
        } else {
            neighbor_missed += 1;
            if truth_louder {
                neighbor_missed_missed_louder += 1;
            } else {
                neighbor_missed_missed_quieter += 1;
            }
        }
    }
    let total = frontier.len();
    println!(
        "  neighbor decoded by pancetta: {} ({:.1}%)",
        neighbor_decoded,
        neighbor_decoded as f64 / total.max(1) as f64 * 100.0
    );
    println!(
        "    → truth is louder (different mechanism): {}",
        neighbor_decoded_missed_louder
    );
    println!(
        "    → truth is quieter (textbook capture-effect): {}",
        neighbor_decoded_missed_quieter
    );
    println!(
        "  neighbor MISSED by pancetta: {} ({:.1}%)",
        neighbor_missed,
        neighbor_missed as f64 / total.max(1) as f64 * 100.0
    );
    println!(
        "    → truth is louder (both missed; sync interference?): {}",
        neighbor_missed_missed_louder
    );
    println!(
        "    → truth is quieter (both missed; weak pair): {}",
        neighbor_missed_missed_quieter
    );

    println!("\n### CC1 interpretation");
    println!("  - 'neighbor decoded + truth louder' = pancetta decoded the WEAKER signal but missed the LOUDER one. Sync/SIC priority bug?");
    println!("  - 'neighbor decoded + truth quieter' = textbook SIC failure: subtracted the strong, didn't recover the weak.");
    println!("  - 'neighbor missed + either' = both signals missed; mutual capture.");

    Ok(())
}
