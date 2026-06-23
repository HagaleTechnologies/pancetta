//! fp_format_audit — hb-058 instrument-before-implement
//!
//! Question: JTDX filters `/R` false decodes, malformed ARRL Field Day
//! messages, and invalid directional CQ. pancetta already has a thorough
//! `is_plausible` post-decode filter (telemetry + freetext rejected,
//! both-portable `/` rejected, payload validated) AND a live callsign-
//! continuity FP filter (hb-052/062). So: how many hard-200 NOVELS
//! (FP-likely) actually fall into hb-058's target categories, and would
//! filtering them cost any RECOVERED (real, jt9-matched) decodes?
//!
//! Method: decode hard-200 with the production config, classify each
//! decode novel-vs-recovered by text match against per-WAV jt9 truth,
//! and tally novel/recovered counts per category. A category that is
//! novel-heavy and recovered-empty is a safe filter target; one with
//! real decodes is a recall risk.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example fp_format_audit

use anyhow::Context;
use pancetta_ft8::message::MessageType;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

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

/// A counted bucket: (novel_count, recovered_count).
#[derive(Default, Clone, Copy)]
struct Bucket {
    novel: usize,
    recovered: usize,
}

fn main() -> anyhow::Result<()> {
    let workspace = workspace_root()?;
    let manifest_path = workspace.join("research/corpus/curated/ft8/hard_200.manifest.json");

    // Per-WAV jt9 truth message sets, keyed by sha. The manifest entries
    // carry baseline_path pointing at the jt9 truth JSON.
    let manifest_str = std::fs::read_to_string(&manifest_path)?;
    let manifest: Value = serde_json::from_str(&manifest_str)?;
    let entries = manifest
        .get("entries")
        .and_then(|e| e.as_array())
        .context("manifest missing entries")?;

    let baselines_dir = workspace.join("research/baselines/ft8");

    eprintln!("Decoding {} hard-200 WAVs...", entries.len());
    let cfg = Ft8Config::default();

    let mut by_type: HashMap<&'static str, Bucket> = HashMap::new();
    let mut slash_r: Bucket = Bucket::default(); // single /R suffix (both-/R already rejected by is_plausible)
    let mut directional_cq: Bucket = Bucket::default(); // CQ <modifier> ...
    let mut total = Bucket::default();

    for (i, entry) in entries.iter().enumerate() {
        if i % 50 == 0 {
            eprintln!("  {}/{}", i, entries.len());
        }
        let wav_path = entry
            .get("wav_path")
            .and_then(|p| p.as_str())
            .context("entry missing wav_path")?;
        let sha = entry
            .get("wav_sha256")
            .and_then(|s| s.as_str())
            .context("entry missing wav_sha256")?;

        // Load this WAV's jt9 truth message set.
        let baseline_path = baselines_dir.join(format!("{sha}.json"));
        let truth: HashSet<String> = match std::fs::read_to_string(&baseline_path) {
            Ok(s) => {
                let v: Value = serde_json::from_str(&s)?;
                v.get("decodes")
                    .and_then(|d| d.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|d| d.get("message").and_then(|m| m.as_str()))
                            .map(|m| m.trim().to_string())
                            .collect()
                    })
                    .unwrap_or_default()
            }
            Err(_) => HashSet::new(),
        };

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

        for msg in decodes {
            let text = msg.text.trim();
            // Match eval's matcher: substring either direction.
            let recovered = truth.iter().any(|t| t.contains(text) || text.contains(t));

            let bump = |b: &mut Bucket| {
                if recovered {
                    b.recovered += 1;
                } else {
                    b.novel += 1;
                }
            };
            bump(&mut total);

            let type_name = match msg.message.message_type {
                MessageType::Standard => "Standard",
                MessageType::Extended => "Extended",
                MessageType::Contest => "Contest",
                MessageType::FieldDay => "FieldDay",
                MessageType::Telemetry => "Telemetry",
                MessageType::FreeText => "FreeText",
                MessageType::DXpedition => "DXpedition",
                MessageType::RTTYRoundup => "RTTYRoundup",
                MessageType::NonStdCall => "NonStdCall",
                MessageType::Unknown => "Unknown",
            };
            bump(by_type.entry(type_name).or_default());

            // /R suffix on any present callsign (single — both-/R is
            // already dropped upstream by is_plausible).
            let calls = [&msg.message.from_callsign, &msg.message.to_callsign];
            let any_slash_r = calls
                .iter()
                .filter_map(|c| c.as_deref())
                .any(|c| c.to_uppercase().ends_with("/R"));
            if any_slash_r {
                bump(&mut slash_r);
            }

            // Directional CQ: text begins "CQ <modifier> ...".
            let upper = text.to_uppercase();
            let toks: Vec<&str> = upper.split_whitespace().collect();
            if toks.len() >= 2 && toks[0] == "CQ" {
                let m = toks[1];
                let is_call = m.len() >= 4 && m.chars().any(|c| c.is_ascii_digit());
                if !is_call {
                    // "CQ <something-not-a-callsign>" = directional/contest CQ
                    bump(&mut directional_cq);
                }
            }
        }
    }

    println!("\n=== hard-200 decode tally (production config, layered BP) ===");
    println!("TOTAL: recovered={} novel={}", total.recovered, total.novel);
    println!("\n--- by message_type (recovered / novel) ---");
    let mut types: Vec<_> = by_type.iter().collect();
    types.sort_by_key(|(k, _)| *k);
    for (k, b) in types {
        println!("  {k:14} rec={:5} novel={:5}", b.recovered, b.novel);
    }
    println!("\n--- hb-058 target categories (recovered / novel) ---");
    println!(
        "  single /R suffix    rec={:5} novel={:5}",
        slash_r.recovered, slash_r.novel
    );
    println!(
        "  directional CQ      rec={:5} novel={:5}",
        directional_cq.recovered, directional_cq.novel
    );
    println!("\nInterpretation: a category with many novels and ~0 recovered");
    println!("is a safe hb-058 filter target. Recovered>0 means a recall risk.");
    Ok(())
}
