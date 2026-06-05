//! Batch 31 / Diagnostic Q — Multi-pattern FP audit (Line A).
//!
//! Audit 5 candidate FP patterns. For each: emissions on noise + hard-200,
//! truths on hard-200, ship-or-shelve decision.
//!
//! Patterns:
//! 1. **self_call**: message addresses our own callsign ("K5ARH") in
//!    callsign-1 position (we're CQ-ing OURSELVES, structurally
//!    nonsensical for autonomous station).
//! 2. **degenerate_grid**: 4-char grid where first two letters are
//!    impossible (AA / ZZ / etc.) or grid pattern matches a noise-typical
//!    distribution.
//! 3. **repeated_in_slot**: same message text emitted twice or more
//!    within the same WAV (LDPC + multipass artifact; recall-preserving
//!    by definition since we'd already have the first instance).
//! 4. **digit_run_callsign**: callsign-2 with 3+ consecutive digits
//!    (e.g., K1234 is unusual; real callsigns have ≤2 consecutive digits).
//! 5. **same_callsign_both_positions**: callsign-1 == callsign-2
//!    (sending a message to yourself).
//!
//! Method: decode full hard-200 + 100 noise windows; bucket each
//! emission by pattern + TP-status; report per-pattern audit numbers.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch31_pattern_audit

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

fn callsign_tokens(message: &str) -> Vec<String> {
    pancetta_qso::callsign_continuity::callsigns_in(message)
}

fn p_self_call(msg: &str, our_call: &str) -> bool {
    let calls = callsign_tokens(msg);
    if calls.is_empty() {
        return false;
    }
    // We "self-call" if callsign-1 is us (we're being addressed)
    // OR callsign-2 is us (we're responding to ourselves — non-CQ)
    calls.iter().any(|c| c.eq_ignore_ascii_case(our_call)) && !msg.to_uppercase().starts_with("CQ ")
}

fn p_degenerate_grid(msg: &str) -> bool {
    // Grid format: 2-char field letters (A-R), 2-digit square. A grid
    // like "AA00" or "RR99" is technically legal but extremely rare on
    // HF. Real grids are biased toward populated continents (mostly DM..GN
    // for NA, IO..KP for EU, PM..QN for Asia).
    for t in msg.split_whitespace() {
        if t.len() == 4 {
            let chars: Vec<char> = t.chars().collect();
            let is_grid_shape = chars[0].is_ascii_alphabetic()
                && chars[1].is_ascii_alphabetic()
                && chars[2].is_ascii_digit()
                && chars[3].is_ascii_digit();
            if is_grid_shape {
                let f0 = chars[0].to_ascii_uppercase();
                let f1 = chars[1].to_ascii_uppercase();
                // Same letter twice and at A or Z extremes
                if f0 == f1 && (f0 == 'A' || f0 == 'Z') {
                    return true;
                }
                // Letters outside A-R (not in legal grid space)
                if !('A'..='R').contains(&f0) || !('A'..='R').contains(&f1) {
                    return true;
                }
            }
        }
    }
    false
}

fn p_digit_run_callsign(msg: &str) -> bool {
    let calls = callsign_tokens(msg);
    for c in calls {
        let mut run = 0;
        let mut max_run = 0;
        for ch in c.chars() {
            if ch.is_ascii_digit() {
                run += 1;
                if run > max_run {
                    max_run = run;
                }
            } else {
                run = 0;
            }
        }
        if max_run >= 3 {
            return true;
        }
    }
    false
}

fn p_same_callsign_twice(msg: &str) -> bool {
    let calls = callsign_tokens(msg);
    calls.len() >= 2 && calls[0].eq_ignore_ascii_case(&calls[1])
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH31_Q_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let n_noise: usize = std::env::var("BATCH31_Q_NOISE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let our_call: String =
        std::env::var("BATCH31_Q_OUR_CALL").unwrap_or_else(|_| "K5ARH".to_string());

    println!("## Batch 31 / Diagnostic Q — multi-pattern FP audit");
    println!(
        "  hard-200 WAVs: {}, noise windows: {}, our_call: {}",
        top_n, n_noise, our_call
    );

    let cfg = Ft8Config::default();

    // pattern_name -> (emissions_total, fp_count, tp_count, truth_count_on_corpus)
    let pattern_names = [
        "self_call",
        "degenerate_grid",
        "repeated_in_slot",
        "digit_run_callsign",
        "same_callsign_twice",
    ];
    let mut counts: HashMap<&str, (usize, usize, usize, usize)> = HashMap::new();
    for n in &pattern_names {
        counts.insert(n, (0, 0, 0, 0));
    }

    let mut total_decodes = 0usize;
    let mut total_tp = 0usize;

    // Hard-200 pass
    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);

        // Count truths matching each pattern (so we know recall cost)
        for t in &truth {
            if p_self_call(t, &our_call) {
                counts.get_mut("self_call").unwrap().3 += 1;
            }
            if p_degenerate_grid(t) {
                counts.get_mut("degenerate_grid").unwrap().3 += 1;
            }
            if p_digit_run_callsign(t) {
                counts.get_mut("digit_run_callsign").unwrap().3 += 1;
            }
            if p_same_callsign_twice(t) {
                counts.get_mut("same_callsign_twice").unwrap().3 += 1;
            }
            // p_repeated_in_slot is per-WAV not per-truth; skip in this loop
        }

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        // Count repeated emissions within this WAV.
        let mut wav_text_counts: HashMap<String, usize> = HashMap::new();
        for d in &decoded {
            *wav_text_counts.entry(d.text.clone()).or_insert(0) += 1;
        }

        for d in decoded {
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
            tally!("self_call", p_self_call(&d.text, &our_call));
            tally!("degenerate_grid", p_degenerate_grid(&d.text));
            tally!(
                "repeated_in_slot",
                wav_text_counts.get(&d.text).copied().unwrap_or(0) >= 2
            );
            tally!("digit_run_callsign", p_digit_run_callsign(&d.text));
            tally!("same_callsign_twice", p_same_callsign_twice(&d.text));
        }
    }

    // Noise pass — only emissions, all FPs by definition
    let mut noise_emissions: HashMap<&str, usize> = HashMap::new();
    for n in &pattern_names {
        noise_emissions.insert(n, 0);
    }
    let mut total_noise_decodes = 0usize;
    for i in 0..n_noise {
        let mut rng = StdRng::seed_from_u64(424242 + i as u64);
        let sigma = 0.05;
        let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&noise)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        let mut wav_text_counts: HashMap<String, usize> = HashMap::new();
        for d in &decoded {
            *wav_text_counts.entry(d.text.clone()).or_insert(0) += 1;
        }
        for d in decoded {
            total_noise_decodes += 1;
            if p_self_call(&d.text, &our_call) {
                *noise_emissions.get_mut("self_call").unwrap() += 1;
            }
            if p_degenerate_grid(&d.text) {
                *noise_emissions.get_mut("degenerate_grid").unwrap() += 1;
            }
            if wav_text_counts.get(&d.text).copied().unwrap_or(0) >= 2 {
                *noise_emissions.get_mut("repeated_in_slot").unwrap() += 1;
            }
            if p_digit_run_callsign(&d.text) {
                *noise_emissions.get_mut("digit_run_callsign").unwrap() += 1;
            }
            if p_same_callsign_twice(&d.text) {
                *noise_emissions.get_mut("same_callsign_twice").unwrap() += 1;
            }
        }
    }

    println!(
        "\n  Total hard-200 decodes: {}  TP: {}  FP: {}",
        total_decodes,
        total_tp,
        total_decodes - total_tp
    );
    println!("  Total noise decodes (all FPs): {}", total_noise_decodes);

    println!(
        "\n  Pattern               | h200 emit | h200 fp | h200 tp | h200 truth | noise emit | FP rate | recall cost"
    );
    println!(
        "  --------------------- | --------- | ------- | ------- | ---------- | ---------- | ------- | -----------"
    );
    for name in pattern_names {
        let (e, fp, tp, truth) = counts[name];
        let noise = noise_emissions[name];
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
        let truth_rate_pct = if truth > 0 {
            truth as f64 / total_decodes.max(1) as f64 * 100.0
        } else {
            0.0
        };
        let _ = truth_rate_pct;
        let verdict = if fp_rate >= 95.0 && recall_cost <= 0.5 {
            "SHIP"
        } else if fp_rate >= 80.0 && recall_cost <= 1.0 {
            "MAYBE"
        } else {
            "shelf"
        };
        println!(
            "  {:<21} | {:>9} | {:>7} | {:>7} | {:>10} | {:>10} | {:>5.1}%  | {:>5.3}% [{}]",
            name, e, fp, tp, truth, noise, fp_rate, recall_cost, verdict
        );
    }

    println!("\n### Ship-or-shelve criteria");
    println!("  SHIP   : FP rate ≥ 95% AND recall cost ≤ 0.5%");
    println!("  MAYBE  : FP rate ≥ 80% AND recall cost ≤ 1.0% — consider with trust-set guard");
    println!("  shelf  : otherwise");

    Ok(())
}
