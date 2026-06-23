//! Batch 87 — re-derive headline precision/miss-rate under hash-normalized
//! truth scoring (hb-248).
//!
//! Batch 86 found that exact-text scoring double-penalizes every pancetta
//! decode of an unresolved hashed callsign (`<...NNNN>` vs ft8_lib's
//! `<...>`): +1 phantom miss and +1 phantom FP each. This probe decodes
//! raw_530_full and hard_1000 at the current production defaults and
//! scores BOTH ways, putting the corrected numbers on record. Also counts
//! the conservative rule's residual: pancetta-RESOLVED hash tokens
//! (`<CALL>`) that still mismatch ft8_lib's `<...>`.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch87_hash_normalized_headlines

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use pancetta_research::metrics::hash_normalize_message;
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

fn load_truth(ws: &Path, sha: &str) -> Vec<String> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{sha}.ft8lib.json"));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return Vec::new();
    };
    v["decodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
        .collect()
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let mut body = String::from(
        "# Batch 87 — headline numbers under hash-normalized scoring (hb-248)\n\n\
         Current production defaults; raw = exact-text scoring (all prior\n\
         batches); norm = `hash_normalize_message` on both sides.\n\n",
    );

    // Default corpora, or a single manifest via PANCETTA_HEADLINE_MANIFEST
    // (e.g. a freshly-captured day) for a quick regression-sentinel pass.
    let default_corpora = ["raw_530_full.manifest.json", "hard_1000.manifest.json"];
    let override_manifest = std::env::var("PANCETTA_HEADLINE_MANIFEST").ok();
    let corpora: Vec<&str> = match &override_manifest {
        Some(m) => vec![m.as_str()],
        None => default_corpora.to_vec(),
    };
    for manifest_name in corpora {
        let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
            ws.join("research/corpus/curated/ft8").join(manifest_name),
        )?)?;
        let entries: Vec<Value> = manifest["entries"]
            .as_array()
            .context("entries")?
            .iter()
            .cloned()
            .collect();
        let label = manifest_name.trim_end_matches(".manifest.json");
        eprintln!("---- {label}: {} entries ----", entries.len());

        let (mut tot, mut tp_raw, mut tp_norm) = (0usize, 0usize, 0usize);
        let (mut truth_n, mut found_raw, mut found_norm) = (0usize, 0usize, 0usize);
        let mut resolved_residual = 0usize;
        for (i, entry) in entries.iter().enumerate() {
            let wav_path = entry["wav_path"].as_str().context("wav_path")?;
            let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
            let samples = match load_wav(Path::new(wav_path)) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let truth_raw: HashSet<String> = load_truth(&ws, sha).into_iter().collect();
            let truth_norm: HashSet<String> = truth_raw
                .iter()
                .map(|t| hash_normalize_message(t))
                .collect();
            let mut decoder = Ft8Decoder::new(Ft8Config::default())
                .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
            let decoded = decoder
                .decode_window(&samples)
                .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
            tot += decoded.len();
            let dec_norm: HashSet<String> = decoded
                .iter()
                .map(|d| hash_normalize_message(&d.text))
                .collect();
            for d in &decoded {
                let norm = hash_normalize_message(&d.text);
                if truth_raw.contains(&d.text) {
                    tp_raw += 1;
                }
                if truth_norm.contains(&norm) {
                    tp_norm += 1;
                } else if norm
                    .split_whitespace()
                    .any(|t| t.starts_with('<') && t.ends_with('>') && t != "<...>")
                {
                    // Pancetta resolved a hash that ft8_lib left
                    // unresolved (or genuinely different decode).
                    resolved_residual += 1;
                }
            }
            truth_n += truth_raw.len();
            for t in &truth_raw {
                if decoded.iter().any(|d| d.text == *t) {
                    found_raw += 1;
                }
            }
            for t in &truth_norm {
                if dec_norm.contains(t) {
                    found_norm += 1;
                }
            }
            if (i + 1) % 400 == 0 {
                eprintln!("    [{label} {}/{}]", i + 1, entries.len());
            }
        }
        let prec_raw = tp_raw as f64 / tot.max(1) as f64;
        let prec_norm = tp_norm as f64 / tot.max(1) as f64;
        let miss_raw = 1.0 - found_raw as f64 / truth_n.max(1) as f64;
        let miss_norm = 1.0 - found_norm as f64 / truth_n.max(1) as f64;
        body.push_str(&format!(
            "## {label}\n\n\
             {tot} decodes, {truth_n} truth messages.\n\n\
             | Scoring | TPs | FPs | Precision | Truth found | Miss rate |\n\
             |---|---:|---:|---:|---:|---:|\n\
             | raw exact-text | {tp_raw} | {} | {prec_raw:.4} | {found_raw} | {:.2}% |\n\
             | hash-normalized | {tp_norm} | {} | {prec_norm:.4} | {found_norm} | {:.2}% |\n\n\
             Resolved-hash residual mismatches (conservative rule's leftover): {resolved_residual}\n\n",
            tot - tp_raw,
            miss_raw * 100.0,
            tot - tp_norm,
            miss_norm * 100.0,
        ));
        println!(
            "{label}: raw prec {prec_raw:.4} miss {:.2}% | norm prec {prec_norm:.4} miss {:.2}% | resolved residual {resolved_residual}", miss_raw*100.0, miss_norm*100.0
        );
    }

    // Default run writes the canonical batch87 note; a manifest override
    // writes a manifest-specific note so it never clobbers the headline.
    let notes_path = match &override_manifest {
        Some(m) => ws.join(format!(
            "research/notes/headline-{}.md",
            m.trim_end_matches(".manifest.json")
        )),
        None => ws.join("research/notes/2026-06-12-batch87-hash-normalized.md"),
    };
    std::fs::write(&notes_path, &body)?;
    println!("wrote {}", notes_path.display());
    Ok(())
}
