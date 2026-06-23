//! resolve_isolated_novels — hb-039 follow-up to hb-024
//!
//! hb-024 found 856 (35.2%) of pancetta's novel decodes on hard_1000
//! were "isolated" — callsigns not seen anywhere in jt9's truth for any
//! of the 1121 baselines. Those novels are ambiguous: rare DX (real
//! but jt9 missed every time) OR LDPC+CRC FPs that happen to produce
//! syntactically-valid callsigns.
//!
//! This probe distinguishes them via SELF-CONSISTENCY. If pancetta
//! finds the same isolated callsign in MULTIPLE WAVs across the corpus,
//! that's evidence of a real but jt9-missed station (FPs almost never
//! reproduce the exact same fake callsign across distinct noise WAVs).
//! If each isolated callsign appears in exactly one pancetta-novel,
//! FP suspicion is high.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example resolve_isolated_novels

use anyhow::Context;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Extract callsigns from an FT8 decoded message. Mirrors the helper in
/// cross_validate_novels.rs.
fn callsigns_in(message: &str) -> Vec<String> {
    let upper = message.to_uppercase();
    let tokens: Vec<&str> = upper.split_whitespace().collect();
    let mut out = Vec::new();
    let mut idx = 0;
    if idx < tokens.len() && tokens[idx] == "CQ" {
        idx += 1;
        if idx < tokens.len() && is_cq_modifier(tokens[idx]) {
            idx += 1;
        }
    }
    for t in tokens.iter().skip(idx).take(2) {
        if looks_like_callsign(t) {
            out.push(t.to_lowercase());
        }
    }
    out
}

fn is_cq_modifier(t: &str) -> bool {
    matches!(t, "DX" | "NA" | "SA" | "EU" | "AS" | "AF" | "OC" | "QRP")
        || t.chars().all(|c| c.is_ascii_digit())
        || (t.len() <= 3 && t.chars().all(|c| c.is_ascii_alphabetic()))
}

fn looks_like_callsign(t: &str) -> bool {
    let len = t.len();
    if !(3..=10).contains(&len) {
        return false;
    }
    let mut has_digit = false;
    let mut has_alpha = false;
    for c in t.chars() {
        if c.is_ascii_digit() {
            has_digit = true;
        } else if c.is_ascii_alphabetic() {
            has_alpha = true;
        } else if c != '/' {
            return false;
        }
    }
    has_digit && has_alpha
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

fn load_wav_samples(path: &PathBuf) -> anyhow::Result<Vec<f32>> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening WAV {}", path.display()))?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.channels == 1 && spec.sample_rate == 12000,
        "WAV {} not 12kHz mono",
        path.display()
    );
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
    };
    Ok(samples)
}

fn main() -> anyhow::Result<()> {
    let workspace = workspace_root()?;
    let baselines_dir = workspace.join("research/baselines/ft8");
    let manifest_path = workspace.join("research/corpus/curated/ft8/hard_1000.manifest.json");

    // Phase 1: jt9 truth callsign set
    eprintln!("Loading jt9 baselines from {}...", baselines_dir.display());
    let mut jt9_callsigns: HashSet<String> = HashSet::new();
    let mut per_wav_truth: HashMap<String, HashSet<String>> = HashMap::new();
    for entry in std::fs::read_dir(&baselines_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let sha = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let s = std::fs::read_to_string(&path)?;
        let v: Value = serde_json::from_str(&s)?;
        let decodes = v
            .get("decodes")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();
        let mut truth_msgs = HashSet::new();
        for d in &decodes {
            if let Some(m) = d.get("message").and_then(|m| m.as_str()) {
                truth_msgs.insert(m.trim().to_string());
                for cs in callsigns_in(m) {
                    jt9_callsigns.insert(cs);
                }
            }
        }
        per_wav_truth.insert(sha, truth_msgs);
    }
    eprintln!("  loaded; {} unique jt9 callsigns", jt9_callsigns.len());

    // Phase 2: decode hard_1000 with pancetta, count per-callsign WAV
    // appearances in NOVEL decodes (those not matching jt9 truth).
    let manifest_str = std::fs::read_to_string(&manifest_path)?;
    let manifest: Value = serde_json::from_str(&manifest_str)?;
    let entries = manifest
        .get("entries")
        .and_then(|e| e.as_array())
        .context("manifest missing entries")?;
    eprintln!("Decoding {} WAVs with pancetta...", entries.len());

    // For each pancetta-novel callsign, the set of WAVs it appeared in
    // (using HashSet to count distinct WAVs, not distinct messages).
    let mut novel_callsign_wavs: HashMap<String, HashSet<String>> = HashMap::new();

    let cfg = Ft8Config::default();
    for (i, entry) in entries.iter().enumerate() {
        if i % 100 == 0 {
            eprintln!("  {}/{}", i, entries.len());
        }
        let wav_path = entry
            .get("wav_path")
            .and_then(|p| p.as_str())
            .context("entry missing wav_path")?;
        let sha = entry
            .get("wav_sha256")
            .and_then(|s| s.as_str())
            .context("entry missing wav_sha256")?
            .to_string();
        let samples = match load_wav_samples(&PathBuf::from(wav_path)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decodes = match decoder.decode_window(&samples) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let truth = per_wav_truth.get(&sha).cloned().unwrap_or_default();
        for msg in decodes {
            let text = msg.text.trim();
            if truth.iter().any(|t| t.contains(text) || text.contains(t)) {
                continue;
            }
            for cs in callsigns_in(text) {
                novel_callsign_wavs
                    .entry(cs)
                    .or_default()
                    .insert(sha.clone());
            }
        }
    }

    // Phase 3: classify each novel-callsign by self-consistency
    let mut isolated_callsigns = 0usize; // not in jt9
    let mut continued_callsigns = 0usize; // in jt9 (validated by hb-024)
    let mut isolated_appears_in_n_wavs: HashMap<&'static str, usize> = HashMap::new();
    let mut isolated_singletons = 0usize;
    let mut isolated_multi = 0usize;

    for (cs, wavs) in &novel_callsign_wavs {
        let n_wavs = wavs.len();
        if jt9_callsigns.contains(cs) {
            continued_callsigns += 1;
        } else {
            isolated_callsigns += 1;
            let bucket = match n_wavs {
                1 => "1 (singleton)",
                2 => "2",
                3..=5 => "3-5",
                6..=10 => "6-10",
                _ => "10+",
            };
            *isolated_appears_in_n_wavs.entry(bucket).or_insert(0) += 1;
            if n_wavs == 1 {
                isolated_singletons += 1;
            } else {
                isolated_multi += 1;
            }
        }
    }

    let total_novel_callsigns = novel_callsign_wavs.len();
    println!();
    println!("hb-039 — self-consistency check on isolated novel callsigns");
    println!("corpus: hard_1000; jt9 baselines: 1121");
    println!();
    println!("Total unique callsigns in pancetta novel decodes: {total_novel_callsigns}");
    println!("  - in jt9 truth (continued, validated by hb-024): {continued_callsigns}");
    println!("  - NOT in jt9 truth (isolated):                    {isolated_callsigns}");
    println!();
    println!("Self-consistency of isolated callsigns (how many distinct pancetta WAVs):");
    let mut buckets: Vec<_> = isolated_appears_in_n_wavs.iter().collect();
    buckets.sort_by_key(|(k, _)| match **k {
        "1 (singleton)" => 0,
        "2" => 1,
        "3-5" => 2,
        "6-10" => 3,
        "10+" => 4,
        _ => 99,
    });
    for (k, v) in &buckets {
        println!("  {k:>15}: {v}");
    }
    println!();
    println!(
        "Isolated singletons (likely FPs):    {isolated_singletons} ({:.1}% of isolated)",
        100.0 * isolated_singletons as f64 / isolated_callsigns.max(1) as f64
    );
    println!(
        "Isolated multi-appearance (likely real but jt9-missed): {isolated_multi} ({:.1}% of isolated)",
        100.0 * isolated_multi as f64 / isolated_callsigns.max(1) as f64
    );
    println!();
    println!("Interpretation:");
    println!("  - Singletons-dominate isolated set → most isolated novels are likely FPs.");
    println!(
        "  - Multi-appearance dominant → most isolated novels are real (rare DX or jt9-skipped)."
    );
    Ok(())
}
