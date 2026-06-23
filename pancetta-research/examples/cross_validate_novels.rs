//! cross_validate_novels — hb-024 QSO-continuity novel validation
//!
//! Question: across the run we've accumulated thousands of "novel"
//! decodes (messages pancetta finds that jt9's truth doesn't include).
//! Are those novels REAL (jt9 missed them — pancetta's sensitivity is
//! understated) or FALSE POSITIVES (LDPC+CRC happened to converge on
//! random-noise candidates)?
//!
//! Method (option (b) from hb-024 bank entry): callsign continuity. If
//! a novel decode contains a callsign that ALSO appears in jt9's truth
//! for some OTHER WAV in the corpus (especially nearby in time), the
//! callsign was demonstrably on the air — the novel decode is likely
//! real. If the callsign is found nowhere else in any jt9 truth across
//! 1000+ WAVs, it's much more likely an FP.
//!
//! Doesn't need JTDX installation or external API. Pure analysis of
//! existing corpus + on-the-fly pancetta decodes.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example cross_validate_novels

use anyhow::Context;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Extract callsigns from an FT8 decoded message. Strips CQ/CQ-modifier
/// prefixes and returns up to 2 callsign-shaped tokens. Returns lowercase
/// for case-insensitive matching.
fn callsigns_in(message: &str) -> Vec<String> {
    let upper = message.to_uppercase();
    let tokens: Vec<&str> = upper.split_whitespace().collect();
    let mut out = Vec::new();
    let mut idx = 0;
    // Skip a leading "CQ" and an optional modifier (CQ DX K1ABC ...).
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

    // ---- Phase 1: load ALL jt9 baselines into memory. Build a global
    // callsign → count map across the entire corpus.
    eprintln!("Loading jt9 baselines from {}...", baselines_dir.display());
    let mut global_callsign_counts: HashMap<String, u32> = HashMap::new();
    let mut per_wav_truth: HashMap<String, HashSet<String>> = HashMap::new(); // sha → message set
    let mut baselines_loaded = 0;
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
                    *global_callsign_counts.entry(cs).or_insert(0) += 1;
                }
            }
        }
        per_wav_truth.insert(sha, truth_msgs);
        baselines_loaded += 1;
    }
    eprintln!(
        "  loaded {baselines_loaded} baselines, {} unique callsigns",
        global_callsign_counts.len()
    );

    // ---- Phase 2: decode each hard_1000 WAV with pancetta (production
    // config) and tally novel decodes + their callsigns.
    let manifest_str = std::fs::read_to_string(&manifest_path)?;
    let manifest: Value = serde_json::from_str(&manifest_str)?;
    let entries = manifest
        .get("entries")
        .and_then(|e| e.as_array())
        .context("manifest missing entries")?;
    eprintln!("Decoding {} WAVs with pancetta...", entries.len());

    let mut total_novels = 0usize;
    let mut continued_novels = 0usize; // callsign seen elsewhere in corpus
    let mut isolated_novels = 0usize; // callsign seen nowhere else
    let mut malformed_novels = 0usize; // couldn't extract a callsign
                                       // Histogram: how many WAVs in the corpus contain the novel decode's callsigns?
    let mut continuity_bucket: HashMap<&'static str, usize> = HashMap::new();

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
            Err(_) => continue, // missing WAV; skip
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
            // Exact match against jt9 truth (matches eval's matcher) → not novel
            if truth.iter().any(|t| t.contains(text) || text.contains(t)) {
                continue;
            }
            total_novels += 1;
            // Extract callsigns and look them up
            let calls = callsigns_in(text);
            if calls.is_empty() {
                malformed_novels += 1;
                continue;
            }
            // Max corpus-wide count across the novel's callsigns
            let max_count = calls
                .iter()
                .map(|c| *global_callsign_counts.get(c).unwrap_or(&0))
                .max()
                .unwrap_or(0);
            // Subtract 0 because our novel didn't add to truth (truth only has jt9's decodes)
            if max_count == 0 {
                isolated_novels += 1;
                *continuity_bucket.entry("0 (isolated)").or_insert(0) += 1;
            } else {
                continued_novels += 1;
                let bucket = match max_count {
                    1 => "1",
                    2..=3 => "2-3",
                    4..=10 => "4-10",
                    11..=50 => "11-50",
                    _ => "50+",
                };
                *continuity_bucket.entry(bucket).or_insert(0) += 1;
            }
        }
    }

    println!();
    println!("hb-024 — novel-decode cross-validation via callsign continuity");
    println!(
        "corpus: hard_1000 ({} WAVs); jt9 baselines: {baselines_loaded}",
        entries.len()
    );
    println!();
    println!("Total pancetta novel decodes (not matched in jt9 truth): {total_novels}");
    println!(
        "  - continued (callsign seen elsewhere in jt9 truth):    {continued_novels} ({:.1}%)",
        100.0 * continued_novels as f64 / total_novels.max(1) as f64
    );
    println!(
        "  - isolated  (callsign NEVER seen in jt9 truth):        {isolated_novels} ({:.1}%)",
        100.0 * isolated_novels as f64 / total_novels.max(1) as f64
    );
    println!(
        "  - malformed (couldn't extract callsign):               {malformed_novels} ({:.1}%)",
        100.0 * malformed_novels as f64 / total_novels.max(1) as f64
    );
    println!();
    println!("Continuity-bucket histogram (# WAVs containing the callsign elsewhere):");
    let mut buckets: Vec<_> = continuity_bucket.iter().collect();
    buckets.sort_by_key(|(k, _)| match **k {
        "0 (isolated)" => 0,
        "1" => 1,
        "2-3" => 2,
        "4-10" => 3,
        "11-50" => 4,
        "50+" => 5,
        _ => 99,
    });
    for (k, v) in &buckets {
        println!("  {k:>14}: {v}");
    }
    println!();
    println!("Interpretation:");
    println!("  - High 'continued' ratio + low 'isolated' → many novels are real → vs_wsjtx_pct is understated");
    println!("  - High 'isolated' ratio → many novels are likely FPs → FP-filter work justified (hb-014, hb-034)");
    println!("  - Mixed → both effects present; investigate per-bucket via the histogram");
    Ok(())
}
