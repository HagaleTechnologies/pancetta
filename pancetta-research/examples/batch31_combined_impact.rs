//! Batch 31 / Diagnostic U — Combined-filter impact on full hard-200.
//!
//! Measures the cumulative impact of all FP filters wired across
//! Batch 30 + Batch 31 on full hard-200 + 100 noise windows:
//!
//!   1. **Pre-batch baseline**: no filter at all (raw decoder output)
//!   2. **hb-062 alone**: callsign-continuity at full hard-200 trust
//!   3. **hb-062 + /R + degenerate_grid + digit_run**
//!      (Batch 30 /R wire + Batch 31 new patterns)
//!   4. **hb-062 + all patterns + hb-103 v1 @ SHIP_PRECISE**
//!      (Stack of every filter we've shipped, fused-score threshold for
//!      autonomous-TX-precision decisions)
//!
//! Each row reports: TP recall, FP reduction vs no-filter baseline.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch31_combined_impact

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use pancetta_qso::{
    callsign_continuity::{has_high_risk_fp_pattern, CallsignContinuityFilter},
    content_score_from_features, ContentFeatures, MessageContentScore,
};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: usize = 12_000;
const SLOT_S: usize = 15;
const WINDOW_SAMPLES: usize = SAMPLE_RATE * SLOT_S;

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
        .map(|a| {
            a.iter()
                .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn load_all_truth_callsigns(ws: &Path) -> Result<HashSet<String>> {
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let mut out = HashSet::new();
    for e in manifest["entries"].as_array().context("entries")? {
        let sha = e["wav_sha256"].as_str().unwrap_or("");
        let bpath = ws
            .join("research/baselines/ft8")
            .join(format!("{}.json", sha));
        let Ok(txt) = std::fs::read_to_string(&bpath) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&txt) else {
            continue;
        };
        let Some(arr) = v["decodes"].as_array() else {
            continue;
        };
        for d in arr {
            if let Some(msg) = d["message"].as_str() {
                for c in pancetta_qso::callsign_continuity::callsigns_in(msg) {
                    out.insert(c);
                }
            }
        }
    }
    Ok(out)
}

fn gaussian_noise(rng: &mut StdRng, n: usize, sigma: f32) -> Vec<f32> {
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let u1: f32 = rng.gen_range(f32::EPSILON..1.0);
        let u2: f32 = rng.gen_range(0.0..1.0);
        let mag = (-2.0 * u1.ln()).sqrt();
        let z0 = mag * (2.0 * std::f32::consts::PI * u2).cos();
        let z1 = mag * (2.0 * std::f32::consts::PI * u2).sin();
        out.push(z0 * sigma);
        i += 1;
        if i < n {
            out.push(z1 * sigma);
            i += 1;
        }
    }
    out
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH31_U_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let n_noise: usize = std::env::var("BATCH31_U_NOISE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);

    println!("## Batch 31 / Diagnostic U — combined filter impact");
    println!("  hard-200 WAVs: {}  noise windows: {}", top_n, n_noise);

    // Build the trust filter at full hard-200 coverage (simulating an
    // operator who has logged everyone in the corpus).
    let trust_calls = load_all_truth_callsigns(&ws)?;
    let mut filter = CallsignContinuityFilter::new(100);
    filter.extend_from_iter(trust_calls.iter().cloned());
    println!("  trust set: {} unique callsigns", trust_calls.len());

    let cfg = Ft8Config::default();

    // Collect every decode with its (text, score, is_tp).
    #[derive(Debug, Clone)]
    struct Entry {
        text: String,
        score: f64,
        is_tp: bool,
    }
    let mut entries_all: Vec<Entry> = Vec::new();

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let wav = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&wav)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in &decoded {
            let score = content_score_from_features(
                ContentFeatures {
                    text: &d.text,
                    confidence: d.confidence,
                    snr_db: d.snr_db,
                    time_offset: d.time_offset,
                    bp_iterations_used: None,
                    osd_depth_used: None,
                    nharderrs: None,
                    min_llr_magnitude: None,
                    lateness_frac: None,
                },
                &filter,
            );
            entries_all.push(Entry {
                text: d.text.clone(),
                score,
                is_tp: truth.contains(&d.text),
            });
        }
    }
    for i in 0..n_noise {
        let mut rng = StdRng::seed_from_u64(311311 + i as u64);
        let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, 0.05);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&noise)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in &decoded {
            let score = content_score_from_features(
                ContentFeatures {
                    text: &d.text,
                    confidence: d.confidence,
                    snr_db: d.snr_db,
                    time_offset: d.time_offset,
                    bp_iterations_used: None,
                    osd_depth_used: None,
                    nharderrs: None,
                    min_llr_magnitude: None,
                    lateness_frac: None,
                },
                &filter,
            );
            entries_all.push(Entry {
                text: d.text.clone(),
                score,
                is_tp: false, // all noise = FP
            });
        }
    }

    let total = entries_all.len();
    let total_tp = entries_all.iter().filter(|e| e.is_tp).count();
    let total_fp = total - total_tp;

    println!(
        "\n  Total decodes: {}  TP: {}  FP: {}\n",
        total, total_tp, total_fp
    );

    // Four filter stacks.
    let runs: Vec<(&str, Box<dyn Fn(&Entry) -> bool>)> = vec![
        ("no_filter", Box::new(|_e: &Entry| true)),
        ("hb-062 alone", Box::new(|e: &Entry| filter.accept(&e.text))),
        (
            "hb-062 + patterns",
            Box::new(|e: &Entry| {
                if has_high_risk_fp_pattern(&e.text) {
                    return false;
                }
                filter.accept(&e.text)
            }),
        ),
        (
            "hb-062 + patterns + hb-103 SHIP_PRECISE",
            Box::new(|e: &Entry| {
                if has_high_risk_fp_pattern(&e.text) {
                    return false;
                }
                if !filter.accept(&e.text) {
                    return false;
                }
                e.score >= MessageContentScore::SHIP_PRECISE
            }),
        ),
    ];

    println!(
        "  {:<43} | TP / total | FP / total | TP recall | FP reduction",
        "Stack"
    );
    println!(
        "  ------------------------------------------- | ---------- | ---------- | --------- | ------------"
    );
    let baseline_fp = total_fp;
    for (name, predicate) in &runs {
        let tp = entries_all
            .iter()
            .filter(|e| e.is_tp && predicate(e))
            .count();
        let fp = entries_all
            .iter()
            .filter(|e| !e.is_tp && predicate(e))
            .count();
        let tp_recall = tp as f64 / total_tp.max(1) as f64 * 100.0;
        let fp_red = (1.0 - fp as f64 / baseline_fp.max(1) as f64) * 100.0;
        println!(
            "  {:<43} | {:>4}/{:<4} | {:>4}/{:<4} |   {:>5.1}% |     {:>+5.1}%",
            name, tp, total_tp, fp, baseline_fp, tp_recall, fp_red
        );
    }

    println!("\n### Verdict");
    println!("  The combined stack characterizes the operational FP filter ceiling.");
    println!("  Production currently runs `hb-062 + patterns` (lines 1 + 3 stacked");
    println!("  via accept()'s pre-gate + trust-set check). The hb-103 score is");
    println!("  ADDITIVE and AVAILABLE but not in the default accept() path —");
    println!("  consumers (autonomous-TX decision, TUI display) opt in.");

    Ok(())
}
