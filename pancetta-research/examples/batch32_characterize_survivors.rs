//! Batch 32 / Diagnostic X — Characterize FPs that survive hb-103
//! SHIP_PRECISE.
//!
//! Of the FPs that the combined stack still emits at the +2.977
//! threshold, what features fooled the score? Bucket by:
//!   - in_trust_set status (both, any, neither)
//!   - confidence range
//!   - time_offset range
//!   - message-type (Standard, Cq, Reply, etc.)
//!   - message_text sample (first 20 each)
//!
//! Informs the next attack surface.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch32_characterize_survivors

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
    let top_n: usize = std::env::var("BATCH32_X_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let n_noise: usize = std::env::var("BATCH32_X_NOISE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    println!("## Batch 32 / Diagnostic X — characterize FPs surviving SHIP_PRECISE");

    let trust = load_all_truth_callsigns(&ws)?;
    let mut filter = CallsignContinuityFilter::new(100);
    filter.extend_from_iter(trust.iter().cloned());

    let cfg = Ft8Config::default();
    let mut surviving_fps: Vec<(String, f64, f32, f64, f32)> = Vec::new();
    // (text, score, confidence, dt, snr)
    let mut total_fps = 0usize;

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
            let is_tp = truth.contains(&d.text);
            if is_tp {
                continue;
            }
            total_fps += 1;
            // Filter out: high-risk pattern, hb-062 reject, score < SHIP_PRECISE
            if has_high_risk_fp_pattern(&d.text) {
                continue;
            }
            if !filter.accept(&d.text) {
                continue;
            }
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
            if score >= MessageContentScore::SHIP_PRECISE {
                surviving_fps.push((d.text.clone(), score, d.confidence, d.time_offset, d.snr_db));
            }
        }
    }
    for i in 0..n_noise {
        let mut rng = StdRng::seed_from_u64(626262 + i as u64);
        let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, 0.05);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&noise)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in &decoded {
            total_fps += 1;
            if has_high_risk_fp_pattern(&d.text) {
                continue;
            }
            if !filter.accept(&d.text) {
                continue;
            }
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
            if score >= MessageContentScore::SHIP_PRECISE {
                surviving_fps.push((d.text.clone(), score, d.confidence, d.time_offset, d.snr_db));
            }
        }
    }

    println!("  total FPs in corpus: {}", total_fps);
    println!(
        "  surviving FPs at SHIP_PRECISE: {} ({:.1}%)",
        surviving_fps.len(),
        surviving_fps.len() as f64 / total_fps.max(1) as f64 * 100.0,
    );

    // Sort by score descending
    surviving_fps.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    println!("\n  Top-30 surviving FPs by score:");
    println!("    {:<32} | score   | conf  | dt    | snr", "text");
    println!("    --------------------------------- | ------- | ----- | ----- | ----");
    for (text, score, conf, dt, snr) in surviving_fps.iter().take(30) {
        println!(
            "    {:<32} | {:>+5.3}  | {:>4.2} | {:>4.2} | {:>+4.1}",
            text.chars().take(32).collect::<String>(),
            score,
            conf,
            dt,
            snr
        );
    }

    println!("\n  Feature distribution of survivors:");
    let conf_high = surviving_fps.iter().filter(|f| f.2 >= 0.9).count();
    let dt_low = surviving_fps.iter().filter(|f| f.3 < 3.0).count();
    let dt_typical = surviving_fps
        .iter()
        .filter(|f| f.3 >= 3.0 && f.3 < 8.0)
        .count();
    let dt_high = surviving_fps.iter().filter(|f| f.3 >= 8.0).count();
    println!("    confidence ≥ 0.9:     {}", conf_high);
    println!("    dt < 3.0s (TP-like):  {}", dt_low);
    println!("    dt 3-8s (mixed):      {}", dt_typical);
    println!("    dt ≥ 8.0s (FP-like):  {}", dt_high);

    println!("\n  These are the FPs hb-103 v1 can't catch.");
    println!("  Next attack surface: pattern-rejects on message text content");
    println!("  (e.g., 'CQ' patterns to specific countries we don't operate in),");
    println!("  or per-decode-time gate (Batch 32 V found FPs are 2.2× slower).");

    Ok(())
}
