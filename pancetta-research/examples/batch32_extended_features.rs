//! Batch 32 / Diagnostic V — extended feature extraction + AUC.
//!
//! Extends Batch 31 R+S with three new features:
//! * `decode_time_into_window` — Option<Duration> on DecodedMessage
//!   (hb-129 instrumentation). Hypothesis: FPs take longer to CRC-pass
//!   because they involve more LDPC iterations / OSD fallback.
//! * `payload_bit_entropy` — Shannon entropy of the 91-bit payload.
//!   Real messages compress (structured callsign + grid + type bits);
//!   random-noise FPs have higher entropy.
//! * `msg_type_code` — `i3`/`n3` message type from `Ft8Message`.
//!   Categorical; some types may be more FP-prone than others.
//!
//! Recomputes per-feature AUC and ranks against the Batch 31 features.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch32_extended_features

use anyhow::{Context, Result};
use bitvec::prelude::BitVec;
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

/// Shannon entropy of a bit sequence in bits (0..1 fraction; max = 1).
fn bit_entropy(bits: &BitVec) -> f64 {
    if bits.is_empty() {
        return 0.0;
    }
    let n = bits.len() as f64;
    let ones = bits.iter().filter(|b| **b).count() as f64;
    let zeros = n - ones;
    let mut h = 0.0;
    if ones > 0.0 {
        let p = ones / n;
        h -= p * p.log2();
    }
    if zeros > 0.0 {
        let p = zeros / n;
        h -= p * p.log2();
    }
    h
}

#[derive(Debug, Clone)]
struct Sample {
    is_tp: bool,
    decode_time_s: Option<f64>,
    payload_entropy: f64,
    msg_type_i3: u8,
}

fn build_sample(d: &DecodedMessage, is_tp: bool) -> Sample {
    let entropy = bit_entropy(&d.message.payload_bits);
    // Extract i3 from message_type discriminant — best-effort categorical
    // proxy. The struct discriminant ordering is roughly i3=0..7 first.
    let msg_type_i3 = match d.message.message_type {
        pancetta_ft8::message::MessageType::Standard => 0,
        pancetta_ft8::message::MessageType::FreeText => 1,
        pancetta_ft8::message::MessageType::DXpedition => 2,
        pancetta_ft8::message::MessageType::FieldDay => 3,
        pancetta_ft8::message::MessageType::Telemetry => 4,
        pancetta_ft8::message::MessageType::Contest => 5,
        pancetta_ft8::message::MessageType::RTTYRoundup => 6,
        pancetta_ft8::message::MessageType::NonStdCall => 7,
        _ => 99,
    };
    Sample {
        is_tp,
        decode_time_s: d.decode_time_into_window.map(|d| d.as_secs_f64()),
        payload_entropy: entropy,
        msg_type_i3,
    }
}

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

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH32_V_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let n_noise: usize = std::env::var("BATCH32_V_NOISE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    println!("## Batch 32 / Diagnostic V — extended feature AUC");
    println!("  hard-200 WAVs: {}  noise: {}", top_n, n_noise);

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
            samples.push(build_sample(d, is_tp));
        }
    }
    for i in 0..n_noise {
        let mut rng = StdRng::seed_from_u64(444444 + i as u64);
        let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, 0.05);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&noise)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in &decoded {
            samples.push(build_sample(d, false));
        }
    }

    let n_total = samples.len();
    let n_tp = samples.iter().filter(|s| s.is_tp).count();
    let n_fp = n_total - n_tp;
    println!("  Corpus: {} samples, {} TP, {} FP", n_total, n_tp, n_fp);

    // Coverage check for decode_time_into_window (may be None for ft8_lib path).
    let n_with_decode_time = samples.iter().filter(|s| s.decode_time_s.is_some()).count();
    let pct_coverage = n_with_decode_time as f64 / n_total as f64 * 100.0;
    println!(
        "  decode_time_into_window populated on {}/{} samples ({:.1}%)",
        n_with_decode_time, n_total, pct_coverage
    );

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

    report("decode_time_s (sec)", &|s| s.decode_time_s.unwrap_or(0.0));
    report("payload_entropy", &|s| s.payload_entropy);
    report("msg_type_i3", &|s| s.msg_type_i3 as f64);

    // Now look at the distribution of msg types in TP vs FP
    println!("\n  msg_type_i3 distribution:");
    let mut type_counts: std::collections::BTreeMap<u8, (usize, usize)> =
        std::collections::BTreeMap::new();
    for s in &samples {
        let row = type_counts.entry(s.msg_type_i3).or_insert((0, 0));
        if s.is_tp {
            row.0 += 1;
        } else {
            row.1 += 1;
        }
    }
    println!("  type  | TP_count | FP_count | TP rate");
    println!("  ----- | -------- | -------- | -------");
    for (t, (tp, fp)) in type_counts {
        let total = tp + fp;
        let rate = if total > 0 {
            tp as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        println!("  {:>3}   | {:>8} | {:>8} | {:>5.1}%", t, tp, fp, rate);
    }

    Ok(())
}
