//! Batch 31 / Diagnostic R+S — Per-decode feature extraction and
//! per-feature TP/FP discrimination.
//!
//! Collects (feature_vec, is_tp) on full hard-200 + N noise windows.
//! For each feature, computes:
//!   - mean(TP) vs mean(FP)
//!   - AUC for binary classification (TP=1, FP=0) using simple threshold
//!     sweep
//!
//! Features extracted from DecodedMessage:
//!   - snr_db
//!   - confidence
//!   - error_corrections (count)
//!   - frequency_offset
//!   - time_offset
//!   - ap_level (categorical)
//!   - msg_type (categorical: from message.message_type)
//!
//! Plus derived features:
//!   - text_len (token count)
//!   - has_grid (boolean)
//!   - has_report (boolean)
//!   - in_trust_set (boolean, against hard-200 truth callsigns)
//!
//! Output:
//!   - per-feature TP/FP distribution
//!   - AUC table
//!   - identifies strongest discriminators for score fusion
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch31_content_features

use anyhow::{Context, Result};
use pancetta_ft8::{DecodedMessage, Ft8Config, Ft8Decoder};
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

#[derive(Debug, Clone)]
struct Sample {
    is_tp: bool,
    snr_db: f32,
    confidence: f32,
    error_corrections: u8,
    frequency_offset: f64,
    time_offset: f64,
    ap_level: u8,
    text_len: usize,
    has_grid: bool,
    has_report: bool,
    in_trust_set: bool,
    in_trust_set_both: bool,
}

fn token_is_grid(t: &str) -> bool {
    let chars: Vec<char> = t.chars().collect();
    chars.len() == 4
        && chars[0].is_ascii_alphabetic()
        && chars[1].is_ascii_alphabetic()
        && chars[2].is_ascii_digit()
        && chars[3].is_ascii_digit()
}

fn build_sample(d: &DecodedMessage, is_tp: bool, trust: &HashSet<String>) -> Sample {
    let text_upper = d.text.to_uppercase();
    let tokens: Vec<&str> = text_upper.split_whitespace().collect();
    let calls = pancetta_qso::callsign_continuity::callsigns_in(&d.text);
    let in_trust_set = calls.iter().any(|c| trust.contains(c));
    let in_trust_set_both = calls.len() >= 2 && calls.iter().all(|c| trust.contains(c));
    Sample {
        is_tp,
        snr_db: d.snr_db,
        confidence: d.confidence,
        error_corrections: d.error_corrections,
        frequency_offset: d.frequency_offset,
        time_offset: d.time_offset,
        ap_level: d.ap_level,
        text_len: tokens.len(),
        has_grid: tokens.iter().any(|t| token_is_grid(t)),
        has_report: tokens
            .iter()
            .any(|t| t.starts_with('-') || t.starts_with('+') || *t == "73" || *t == "RR73"),
        in_trust_set,
        in_trust_set_both,
    }
}

/// Compute AUC for a continuous feature `f(sample) -> f64` separating
/// TPs from FPs. Uses the Wilcoxon-Mann-Whitney equivalent (count pairs
/// where TP score > FP score, divided by total pairs).
fn auc_continuous(samples: &[Sample], f: impl Fn(&Sample) -> f64) -> f64 {
    let tps: Vec<f64> = samples.iter().filter(|s| s.is_tp).map(&f).collect();
    let fps: Vec<f64> = samples.iter().filter(|s| !s.is_tp).map(&f).collect();
    if tps.is_empty() || fps.is_empty() {
        return 0.5;
    }
    let mut count_higher = 0u64;
    let mut count_tied = 0u64;
    for &t in &tps {
        for &fp in &fps {
            if t > fp {
                count_higher += 1;
            } else if t == fp {
                count_tied += 1;
            }
        }
    }
    let total = (tps.len() * fps.len()) as u64;
    (count_higher as f64 + 0.5 * count_tied as f64) / total as f64
}

/// AUC for a boolean feature; symmetric (return >= 0.5 by flipping if
/// needed).
fn auc_boolean(samples: &[Sample], f: impl Fn(&Sample) -> bool) -> f64 {
    let a = auc_continuous(samples, |s| if f(s) { 1.0 } else { 0.0 });
    a.max(1.0 - a)
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH31_R_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let n_noise: usize = std::env::var("BATCH31_R_NOISE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    println!("## Batch 31 / Diagnostic R+S — per-decode feature extraction + AUC");
    println!("  hard-200 WAVs: {}  noise windows: {}", top_n, n_noise);

    let trust = load_all_truth_callsigns(&ws)?;
    println!("  trust callsign set: {} unique", trust.len());

    let cfg = Ft8Config::default();
    let mut samples: Vec<Sample> = Vec::new();

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
            samples.push(build_sample(d, is_tp, &trust));
        }
    }

    let n_corpus = samples.len();
    let n_corpus_tp = samples.iter().filter(|s| s.is_tp).count();

    for i in 0..n_noise {
        let mut rng = StdRng::seed_from_u64(979797 + i as u64);
        let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, 0.05);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&noise)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in &decoded {
            // All noise emissions are FPs by definition
            samples.push(build_sample(d, false, &trust));
        }
    }

    let n_total = samples.len();
    let n_tp = samples.iter().filter(|s| s.is_tp).count();
    let n_fp = n_total - n_tp;
    println!(
        "\n  Corpus: {} samples, {} TP, {} FP  (hard-200 alone: {} samples, {} TP)",
        n_total, n_tp, n_fp, n_corpus, n_corpus_tp
    );

    // Per-feature AUC.
    println!("\n  Feature             | TP_mean   | FP_mean   | AUC   | Note");
    println!("  ------------------- | --------- | --------- | ----- | ----");

    let report = |name: &str, f: &dyn Fn(&Sample) -> f64| {
        let tp_mean: f64 =
            samples.iter().filter(|s| s.is_tp).map(f).sum::<f64>() / n_tp.max(1) as f64;
        let fp_mean: f64 =
            samples.iter().filter(|s| !s.is_tp).map(f).sum::<f64>() / n_fp.max(1) as f64;
        let auc = auc_continuous(&samples, f);
        let note = if auc >= 0.7 || auc <= 0.3 {
            "**strong**"
        } else if auc >= 0.6 || auc <= 0.4 {
            "moderate"
        } else {
            "weak"
        };
        println!(
            "  {:<19} | {:>9.4} | {:>9.4} | {:>5.3} | {}",
            name, tp_mean, fp_mean, auc, note
        );
    };

    report("snr_db", &|s| s.snr_db as f64);
    report("confidence", &|s| s.confidence as f64);
    report("error_corrections", &|s| s.error_corrections as f64);
    report("frequency_offset", &|s| s.frequency_offset);
    report("time_offset", &|s| s.time_offset);
    report("ap_level", &|s| s.ap_level as f64);
    report("text_len", &|s| s.text_len as f64);
    report("has_grid", &|s| if s.has_grid { 1.0 } else { 0.0 });
    report("has_report", &|s| if s.has_report { 1.0 } else { 0.0 });
    report("in_trust_set (any)", &|s| {
        if s.in_trust_set {
            1.0
        } else {
            0.0
        }
    });
    report("in_trust_set (both)", &|s| {
        if s.in_trust_set_both {
            1.0
        } else {
            0.0
        }
    });

    println!("\n### Per-feature AUC summary");
    println!("  AUC >= 0.7 = strong discriminator (worth fusing)");
    println!("  AUC in [0.6, 0.7] = moderate (incremental contribution)");
    println!("  AUC < 0.6 = weak (mostly correlated with others, low value)");
    println!();
    println!("  Note: AUC = 0.5 means random; the 'AUC values' shown reflect");
    println!("  the probability that a TP scores HIGHER than an FP for the");
    println!("  given feature. For boolean features, the AUC may collapse");
    println!("  near 0.5 if the feature is uncorrelated with TP/FP.");

    Ok(())
}
