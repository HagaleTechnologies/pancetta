//! Batch 36 / Items C2 + C3 + D2 — slot-edge + callsign-length coverage.
//!
//! C2: bucket hard-200 truths by dt_s; recall in each bucket.
//! C3: silence-pad variant sweep — try 0.5s / 1.0s / 1.5s prefix pads
//!     and count net TP gain/loss for each; revisits Batch 35 A which
//!     used 2s and lost 1269 TPs from non-negative-dt positions.
//! D2: bucket truths by callsign-1 length; recall in each bucket.
//!     Batch 34 found callsign-length ≥7 = 0% recall (compound blind
//!     spot). Does hb-219 /R→/P shift this?
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch36_coverage

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

const SAMPLE_RATE: usize = 12_000;

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

fn load_truth(ws: &Path, sha: &str) -> Vec<(String, f64)> {
    // (text, dt_s)
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
        .filter_map(|d| Some((d["message"].as_str()?.to_string(), d["dt_s"].as_f64()?)))
        .collect()
}

fn dt_bucket(dt: f64) -> &'static str {
    if dt < -1.5 {
        "<-1.5"
    } else if dt < -1.0 {
        "-1.5..-1.0"
    } else if dt < -0.5 {
        "-1.0..-0.5"
    } else if dt < 0.0 {
        "-0.5..0"
    } else if dt < 0.5 {
        "0..0.5"
    } else if dt < 1.0 {
        "0.5..1.0"
    } else if dt < 1.5 {
        "1.0..1.5"
    } else if dt < 2.0 {
        "1.5..2.0"
    } else {
        ">=2.0"
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

fn run_decode(samples: &[f32]) -> Result<HashSet<String>> {
    let cfg = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(cfg).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(decoded.into_iter().map(|d| d.text).collect())
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH36_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    println!("## Batch 36 — C2 + C3 + D2 coverage");

    let mut dt_truth: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut cs_truth: BTreeMap<usize, (usize, usize)> = BTreeMap::new();

    // C3 pad-size totals: pad_samples → (gained_TPs, lost_TPs)
    let pad_sizes: Vec<usize> = vec![
        (SAMPLE_RATE as f64 * 0.5) as usize,
        SAMPLE_RATE,
        (SAMPLE_RATE as f64 * 1.5) as usize,
    ];
    let pad_labels = ["0.5s", "1.0s", "1.5s"];
    let mut pad_gained = vec![0usize; pad_sizes.len()];
    let mut pad_lost = vec![0usize; pad_sizes.len()];

    // limit C3 to first 50 WAVs (slow because 4 decodes each)
    let c3_limit = top_n.min(50);

    for (idx, entry) in entries.iter().take(top_n).enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let truth_set: HashSet<String> = truth.iter().map(|t| t.0.clone()).collect();

        let baseline_decoded = run_decode(&samples)?;

        // C2 dt buckets
        for (text, dt) in &truth {
            let bucket = dt_bucket(*dt);
            let row = dt_truth.entry(bucket).or_insert((0, 0));
            row.0 += 1;
            if baseline_decoded.contains(text) {
                row.1 += 1;
            }
        }

        // D2 callsign-length buckets
        for (text, _) in &truth {
            if let Some(c) = first_callsign(text) {
                let row = cs_truth.entry(c.len()).or_insert((0, 0));
                row.0 += 1;
                if baseline_decoded.contains(text) {
                    row.1 += 1;
                }
            }
        }

        // C3 pad-size sweep on first c3_limit WAVs
        if idx < c3_limit {
            for (i, &pad) in pad_sizes.iter().enumerate() {
                let mut padded = vec![0.0f32; pad];
                padded.extend_from_slice(&samples);
                let padded_decoded = run_decode(&padded)?;
                for t in &padded_decoded {
                    if truth_set.contains(t) && !baseline_decoded.contains(t) {
                        pad_gained[i] += 1;
                    }
                }
                for t in &baseline_decoded {
                    if truth_set.contains(t) && !padded_decoded.contains(t) {
                        pad_lost[i] += 1;
                    }
                }
            }
        }
    }

    println!("\n### C2 — recall by dt_s bucket");
    println!(
        "  {:<12} {:>6} {:>10} {:>8}",
        "dt_s", "truth", "recovered", "recall"
    );
    let mut dt_items: Vec<_> = dt_truth.iter().collect();
    dt_items.sort_by_key(|(_, (t, _))| std::cmp::Reverse(*t));
    for (bucket, (t, r)) in dt_items {
        let pct = *r as f64 / (*t).max(1) as f64 * 100.0;
        println!("  {:<12} {:>6} {:>10} {:>7.1}%", bucket, t, r, pct);
    }

    println!(
        "\n### C3 — silence-pad variant sweep (first {} WAVs)",
        c3_limit
    );
    println!(
        "  {:<6} {:>10} {:>10} {:>8}",
        "pad", "gained", "lost", "net"
    );
    for (i, label) in pad_labels.iter().enumerate() {
        let net = pad_gained[i] as i64 - pad_lost[i] as i64;
        println!(
            "  {:<6} {:>10} {:>10} {:>+8}",
            label, pad_gained[i], pad_lost[i], net
        );
    }

    println!("\n### D2 — recall by callsign-1 length");
    println!(
        "  {:<6} {:>6} {:>10} {:>8}",
        "len", "truth", "recovered", "recall"
    );
    for (len, (t, r)) in &cs_truth {
        let pct = *r as f64 / (*t).max(1) as f64 * 100.0;
        println!("  {:<6} {:>6} {:>10} {:>7.1}%", len, t, r, pct);
    }

    Ok(())
}
