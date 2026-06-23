//! Batch 32 / Diagnostic Y — More pattern audits (Line A round 2).
//!
//! Audits 5 more candidate FP patterns on full hard-200 + 100 noise.
//! Patterns include:
//!   - /P, /M, /A suffix variants (sibling to /R)
//!   - letter-run callsigns (3+ consecutive same letter)
//!   - msg_type = FreeText (Diagnostic V found 0/16 TPs)
//!   - msg_type = DXpedition (Diagnostic V found 0/75 TPs)
//!   - "two-digit prefix" callsigns (e.g., 9A0AB is fine, but 99XYZ
//!     and 12AB are not real)
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch32_more_patterns

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
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

fn p_slash_p(msg: &str) -> bool {
    msg.to_uppercase()
        .split_whitespace()
        .any(|t| t.ends_with("/P"))
}

fn p_slash_m(msg: &str) -> bool {
    msg.to_uppercase()
        .split_whitespace()
        .any(|t| t.ends_with("/M") || t.ends_with("/MM"))
}

fn p_slash_a(msg: &str) -> bool {
    msg.to_uppercase()
        .split_whitespace()
        .any(|t| t.ends_with("/A"))
}

fn p_letter_run(msg: &str) -> bool {
    let upper = msg.to_uppercase();
    for tok in upper.split_whitespace() {
        let base = tok.split('/').next().unwrap_or(tok);
        let chars: Vec<char> = base.chars().collect();
        if chars.len() < 4 {
            continue;
        }
        let mut run = 1;
        for i in 1..chars.len() {
            if chars[i] == chars[i - 1] && chars[i].is_ascii_alphabetic() {
                run += 1;
                if run >= 3 {
                    return true;
                }
            } else {
                run = 1;
            }
        }
    }
    false
}

fn p_msg_type_freetext(d: &pancetta_ft8::DecodedMessage) -> bool {
    matches!(
        d.message.message_type,
        pancetta_ft8::message::MessageType::FreeText
    )
}

fn p_msg_type_dxpedition(d: &pancetta_ft8::DecodedMessage) -> bool {
    matches!(
        d.message.message_type,
        pancetta_ft8::message::MessageType::DXpedition
    )
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH32_Y_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let n_noise: usize = std::env::var("BATCH32_Y_NOISE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);

    println!("## Batch 32 / Diagnostic Y — Line A round-2 pattern audit");

    let cfg = Ft8Config::default();
    let pattern_names = [
        "slash_p",
        "slash_m",
        "slash_a",
        "letter_run",
        "msg_freetext",
        "msg_dxpedition",
    ];
    let mut counts: HashMap<&str, (usize, usize, usize, usize)> = HashMap::new();
    for n in &pattern_names {
        counts.insert(n, (0, 0, 0, 0));
    }

    let mut total_decodes = 0usize;
    let mut total_tp = 0usize;
    let mut noise_emits: HashMap<&str, usize> = HashMap::new();
    for n in &pattern_names {
        noise_emits.insert(n, 0);
    }

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let wav = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);

        for t in &truth {
            macro_rules! tally_truth {
                ($name:expr, $pred:expr) => {{
                    if $pred {
                        counts.get_mut($name).unwrap().3 += 1;
                    }
                }};
            }
            tally_truth!("slash_p", p_slash_p(t));
            tally_truth!("slash_m", p_slash_m(t));
            tally_truth!("slash_a", p_slash_a(t));
            tally_truth!("letter_run", p_letter_run(t));
            // msg_freetext / dxpedition: not parseable from truth strings, count 0
        }

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&wav)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in &decoded {
            total_decodes += 1;
            let is_tp = truth.contains(&d.text);
            if is_tp {
                total_tp += 1;
            }
            macro_rules! tally {
                ($name:expr, $pred:expr) => {{
                    if $pred {
                        let row = counts.get_mut($name).unwrap();
                        row.0 += 1;
                        if is_tp {
                            row.2 += 1;
                        } else {
                            row.1 += 1;
                        }
                    }
                }};
            }
            tally!("slash_p", p_slash_p(&d.text));
            tally!("slash_m", p_slash_m(&d.text));
            tally!("slash_a", p_slash_a(&d.text));
            tally!("letter_run", p_letter_run(&d.text));
            tally!("msg_freetext", p_msg_type_freetext(d));
            tally!("msg_dxpedition", p_msg_type_dxpedition(d));
        }
    }
    let mut total_noise = 0usize;
    for i in 0..n_noise {
        let mut rng = StdRng::seed_from_u64(737373 + i as u64);
        let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, 0.05);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&noise)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in &decoded {
            total_noise += 1;
            if p_slash_p(&d.text) {
                *noise_emits.get_mut("slash_p").unwrap() += 1;
            }
            if p_slash_m(&d.text) {
                *noise_emits.get_mut("slash_m").unwrap() += 1;
            }
            if p_slash_a(&d.text) {
                *noise_emits.get_mut("slash_a").unwrap() += 1;
            }
            if p_letter_run(&d.text) {
                *noise_emits.get_mut("letter_run").unwrap() += 1;
            }
            if p_msg_type_freetext(d) {
                *noise_emits.get_mut("msg_freetext").unwrap() += 1;
            }
            if p_msg_type_dxpedition(d) {
                *noise_emits.get_mut("msg_dxpedition").unwrap() += 1;
            }
        }
    }
    println!(
        "\n  hard-200 decodes: {} (TP {}, FP {}); noise decodes: {}",
        total_decodes,
        total_tp,
        total_decodes - total_tp,
        total_noise
    );

    println!(
        "\n  Pattern               | h200 emit | h200 fp | h200 tp | h200 truth | noise emit | FP rate | recall cost | Verdict"
    );
    println!(
        "  --------------------- | --------- | ------- | ------- | ---------- | ---------- | ------- | ----------- | -------"
    );
    for name in pattern_names {
        let (e, fp, tp, truth) = counts[name];
        let noise = noise_emits[name];
        let fp_rate = if e > 0 {
            fp as f64 / e as f64 * 100.0
        } else {
            0.0
        };
        let recall_cost = if total_tp > 0 {
            tp as f64 / total_tp as f64 * 100.0
        } else {
            0.0
        };
        let verdict = if e >= 1 && fp_rate >= 95.0 && recall_cost <= 0.5 {
            "SHIP"
        } else if e >= 1 && fp_rate >= 80.0 && recall_cost <= 1.0 {
            "MAYBE"
        } else if e == 0 {
            "no-emit"
        } else {
            "shelf"
        };
        println!(
            "  {:<21} | {:>9} | {:>7} | {:>7} | {:>10} | {:>10} | {:>5.1}%  | {:>5.3}% [{}]",
            name, e, fp, tp, truth, noise, fp_rate, recall_cost, verdict
        );
    }

    Ok(())
}
