//! Batch 66 — per-slot decoder scan with JSONL output.
//!
//! Walks a directory of WAVs (filtered by substring), runs pancetta-ft8
//! on each, and emits one JSON line per slot to stdout. Each line:
//!
//!   {"path":"...","timestamp":<epoch>,"decodes":[{"text":"...",
//!     "freq":1234.5,"snr_db":-10.0,"dt":0.5}],"callsigns":["K1ABC",...]}
//!
//! Downstream Python/Rust can compose a manifest by selecting slots
//! that match criteria (repeat-heavy, QSO-continuous, etc.).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch66_slot_scan -- \
//!     --dir ~/.pancetta/recordings --filter ft8_20260530_ \
//!     > research/corpus/scans/raw_20260530_scan.jsonl

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::{json, Value};
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

fn extract_callsigns(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        if token.eq_ignore_ascii_case("CQ") || token.starts_with('<') {
            continue;
        }
        let cleaned: String = token
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '/')
            .to_uppercase();
        let canonical = cleaned.split('/').next().unwrap_or(&cleaned).to_string();
        if canonical.len() < 3 || canonical.len() > 7 {
            continue;
        }
        let chars: Vec<char> = canonical.chars().collect();
        let mut i = 0;
        let mut prefix_letters = 0;
        while i < chars.len() && chars[i].is_ascii_alphabetic() {
            prefix_letters += 1;
            i += 1;
            if prefix_letters > 2 {
                break;
            }
        }
        if !(1..=2).contains(&prefix_letters) {
            continue;
        }
        let mut digits = 0;
        while i < chars.len() && chars[i].is_ascii_digit() {
            digits += 1;
            i += 1;
            if digits > 4 {
                break;
            }
        }
        if digits == 0 || digits > 4 {
            continue;
        }
        let mut suffix_letters = 0;
        while i < chars.len() && chars[i].is_ascii_alphabetic() {
            suffix_letters += 1;
            i += 1;
            if suffix_letters > 3 {
                break;
            }
        }
        if !(1..=3).contains(&suffix_letters) {
            continue;
        }
        if i != chars.len() {
            continue;
        }
        out.push(canonical);
    }
    out
}

fn timestamp_from_filename(path: &Path) -> Option<i64> {
    let stem = path.file_stem()?.to_str()?;
    let stripped = stem.strip_prefix("ft8_")?;
    let parts: Vec<&str> = stripped.split('_').collect();
    if parts.len() < 2 {
        return None;
    }
    let date = parts[0];
    let time = parts[1];
    if date.len() != 8 || time.len() != 6 {
        return None;
    }
    let year: i64 = date[0..4].parse().ok()?;
    let month: i64 = date[4..6].parse().ok()?;
    let day: i64 = date[6..8].parse().ok()?;
    let hour: i64 = time[0..2].parse().ok()?;
    let min: i64 = time[2..4].parse().ok()?;
    let sec: i64 = time[4..6].parse().ok()?;
    let approx_epoch = (year - 1970) * 365 * 86400
        + (month - 1) * 30 * 86400
        + (day - 1) * 86400
        + hour * 3600
        + min * 60
        + sec;
    Some(approx_epoch)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut dir: Option<String> = None;
    let mut filter: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                dir = Some(args[i + 1].clone());
                i += 2;
            }
            "--filter" => {
                filter = Some(args[i + 1].clone());
                i += 2;
            }
            "--limit" => {
                limit = Some(args[i + 1].parse()?);
                i += 2;
            }
            _ => i += 1,
        }
    }
    let dir = dir.context("missing --dir")?;
    let cfg = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        ..Ft8Config::default()
    };

    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("wav") {
            continue;
        }
        if let Some(f) = &filter {
            if !p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .contains(f)
            {
                continue;
            }
        }
        entries.push(p);
    }
    entries.sort();
    if let Some(l) = limit {
        entries.truncate(l);
    }

    eprintln!("scanning {} slots…", entries.len());
    let t0 = std::time::Instant::now();
    for (i, path) in entries.iter().enumerate() {
        let samples = match load_wav(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        let ts = timestamp_from_filename(path);
        let mut callsigns: std::collections::BTreeSet<String> = Default::default();
        let decode_array: Vec<Value> = decoded
            .iter()
            .map(|d| {
                let cs = extract_callsigns(&d.text);
                for c in &cs {
                    callsigns.insert(c.clone());
                }
                json!({
                    "text": d.text,
                    "freq": d.frequency_offset,
                    "snr_db": d.snr_db,
                    "dt": d.time_offset,
                    "callsigns": cs,
                })
            })
            .collect();
        let line = json!({
            "path": path.to_string_lossy(),
            "timestamp": ts,
            "decodes": decode_array,
            "callsigns_in_slot": callsigns.iter().collect::<Vec<_>>(),
        });
        println!("{line}");
        if (i + 1) % 250 == 0 {
            eprintln!(
                "  [{}/{}] {:.1}s elapsed",
                i + 1,
                entries.len(),
                t0.elapsed().as_secs_f64()
            );
        }
    }
    let _ = workspace_root();
    eprintln!("done in {:.1}s", t0.elapsed().as_secs_f64());
    Ok(())
}
