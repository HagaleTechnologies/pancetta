//! Batch 41 — bounded probes from a diverse backlog.
//!
//! Items 1-5: probes that flip well-defined Ft8Config knobs and measure
//! TP delta on hard-200. Plus diagnostic on the 130-truth mid-slot
//! isolated strong-miss subset from Batch 40.
//!
//! - 130-mid: detailed audit of mid-slot isolated strong-misses
//! - ldpc_iter_sweep: ldpc_iterations ∈ {default, 200, 400}
//! - osd_depth_sweep: osd_depth ∈ {None, Some(1), Some(2), Some(3), Some(4)}
//! - dither: add ε·Gaussian to each WAV pre-decode
//! - dc_remove: subtract mean from WAV pre-decode
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch41_probes

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use rand::Rng;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
struct Truth {
    text: String,
    freq_hz: f64,
    dt_s: f64,
    snr_db: f64,
}

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

fn load_truths(ws: &Path, sha: &str) -> Vec<Truth> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
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
        .filter_map(|d| {
            Some(Truth {
                text: d["message"].as_str()?.to_string(),
                freq_hz: d["freq_hz"].as_f64()?,
                dt_s: d["dt_s"].as_f64().unwrap_or(0.0),
                snr_db: d["snr_db"].as_f64().unwrap_or(0.0),
            })
        })
        .collect()
}

fn classify_msg(text: &str) -> &'static str {
    let upper = text.to_uppercase();
    let tokens: Vec<&str> = upper.split_whitespace().collect();
    if tokens.is_empty() {
        return "empty";
    }
    if tokens[0] == "CQ" {
        return "cq";
    }
    let last = tokens.last().copied().unwrap_or("");
    if last == "73" {
        return "73";
    }
    if last == "RR73" {
        return "rr73";
    }
    if last == "RRR" {
        return "rrr";
    }
    if last.starts_with('-') || last.starts_with('+') {
        return "report";
    }
    if last.starts_with('R')
        && last.len() >= 3
        && last[1..].starts_with(|c: char| c == '-' || c == '+')
    {
        return "report_r";
    }
    if last.len() == 4 {
        let chars: Vec<char> = last.chars().collect();
        if chars[0].is_ascii_alphabetic()
            && chars[1].is_ascii_alphabetic()
            && chars[2].is_ascii_digit()
            && chars[3].is_ascii_digit()
        {
            return "grid";
        }
    }
    "other"
}

fn run_pass(
    entries: &[Value],
    cfg: Ft8Config,
    sample_xform: impl Fn(&mut [f32]),
) -> Result<(HashMap<String, HashSet<String>>, usize, usize)> {
    let ws = workspace_root()?;
    let mut sha_to_decoded: HashMap<String, HashSet<String>> = HashMap::new();
    let mut total_decodes = 0usize;
    let mut total_tps = 0usize;
    for entry in entries.iter() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let mut samples = load_wav(Path::new(wav_path))?;
        sample_xform(&mut samples);
        let truths: HashSet<String> = load_truths(&ws, sha).into_iter().map(|t| t.text).collect();
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        total_decodes += decoded.len();
        for d in &decoded {
            if truths.contains(&d.text) {
                total_tps += 1;
            }
        }
        sha_to_decoded.insert(
            sha.to_string(),
            decoded.into_iter().map(|d| d.text).collect(),
        );
    }
    Ok((sha_to_decoded, total_decodes, total_tps))
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();

    println!("## Batch 41 — bounded probes from a diverse backlog");

    let mut cfg_mp2 = Ft8Config::default();
    cfg_mp2.max_decode_passes = 2;
    let baseline_cfg = cfg_mp2.clone();
    eprintln!("baseline (mp=2)…");
    let (baseline_decoded, baseline_total, baseline_tps) =
        run_pass(&entries, baseline_cfg.clone(), |_| {})?;
    println!(
        "\n### Baseline (mp=2): {} decodes / {} TPs",
        baseline_total, baseline_tps
    );

    // === 130 mid-slot strong-miss audit ===
    println!("\n### 130 mid-slot strong-miss — detailed bucketing");
    let mut mid_slot: Vec<(String, Truth)> = Vec::new();
    for entry in entries.iter() {
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let truths = load_truths(&ws, sha);
        let decoded = baseline_decoded.get(sha).cloned().unwrap_or_default();
        for t in &truths {
            if t.snr_db < -10.0 {
                continue;
            }
            if decoded.contains(&t.text) {
                continue;
            }
            // No neighbor
            let has_neighbor = truths.iter().any(|n| {
                n.text != t.text
                    && (n.freq_hz - t.freq_hz).abs() <= 25.0
                    && (n.dt_s - t.dt_s).abs() <= 1.5
            });
            if has_neighbor {
                continue;
            }
            // Mid-slot only (dt 0..2.0, exclude slot-edges)
            if t.dt_s >= 0.0 && t.dt_s < 2.0 {
                mid_slot.push((sha.to_string(), t.clone()));
            }
        }
    }
    println!("  mid-slot isolated strong-misses: {}", mid_slot.len());
    let mut snr_b: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (_, t) in &mid_slot {
        let b = if t.snr_db >= 0.0 {
            ">=0"
        } else if t.snr_db >= -5.0 {
            "-5..0"
        } else {
            "-10..-5"
        };
        *snr_b.entry(b).or_insert(0) += 1;
    }
    println!("  by SNR:");
    for (k, v) in &snr_b {
        println!("    {:<10} {}", k, v);
    }
    let mut type_b: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (_, t) in &mid_slot {
        *type_b.entry(classify_msg(&t.text)).or_insert(0) += 1;
    }
    let mut tv: Vec<_> = type_b.iter().collect();
    tv.sort_by(|a, b| b.1.cmp(a.1));
    println!("  by msg-type:");
    for (k, v) in tv {
        println!("    {:<10} {}", k, v);
    }

    let recover_count = |decoded_set: &HashMap<String, HashSet<String>>| -> usize {
        mid_slot
            .iter()
            .filter(|(sha, t)| {
                decoded_set
                    .get(sha)
                    .map(|s| s.contains(&t.text))
                    .unwrap_or(false)
            })
            .count()
    };

    // === LDPC iterations sweep ===
    println!("\n### LDPC iterations sweep");
    for iters in [200usize, 400] {
        let mut cfg = baseline_cfg.clone();
        cfg.ldpc_iterations = iters;
        eprintln!("  ldpc_iterations={}…", iters);
        let (decoded, total, tps) = run_pass(&entries, cfg, |_| {})?;
        println!(
            "  iters={:<4}: {} decodes / {} TPs (Δ {:+} TPs); mid-slot recovered: {}/{}",
            iters,
            total,
            tps,
            tps as i64 - baseline_tps as i64,
            recover_count(&decoded),
            mid_slot.len()
        );
    }

    // === OSD depth sweep ===
    println!("\n### OSD depth sweep");
    for osd in [None, Some(0), Some(1), Some(3), Some(4)] {
        let mut cfg = baseline_cfg.clone();
        cfg.osd_depth = osd;
        eprintln!("  osd_depth={:?}…", osd);
        let (decoded, total, tps) = run_pass(&entries, cfg, |_| {})?;
        println!(
            "  osd={:?}: {} decodes / {} TPs (Δ {:+} TPs); mid-slot recovered: {}/{}",
            osd,
            total,
            tps,
            tps as i64 - baseline_tps as i64,
            recover_count(&decoded),
            mid_slot.len()
        );
    }

    // === Dither pre-decode ===
    println!("\n### Pre-decode dither (additive Gaussian)");
    for amp in [0.001f32, 0.005, 0.01] {
        eprintln!("  dither amp={}…", amp);
        let (decoded, total, tps) = run_pass(&entries, baseline_cfg.clone(), |s| {
            let mut rng = rand::thread_rng();
            for x in s.iter_mut() {
                let g: f32 = rng.gen_range(-1.0..1.0);
                *x += g * amp;
            }
        })?;
        println!(
            "  amp={:<6}: {} decodes / {} TPs (Δ {:+} TPs); mid-slot recovered: {}/{}",
            amp,
            total,
            tps,
            tps as i64 - baseline_tps as i64,
            recover_count(&decoded),
            mid_slot.len()
        );
    }

    // === DC offset removal ===
    println!("\n### Pre-decode DC offset removal");
    eprintln!("  dc-remove…");
    let (decoded, total, tps) = run_pass(&entries, baseline_cfg.clone(), |s| {
        let mean = s.iter().copied().sum::<f32>() / s.len() as f32;
        for x in s.iter_mut() {
            *x -= mean;
        }
    })?;
    println!(
        "  dc_remove: {} decodes / {} TPs (Δ {:+} TPs); mid-slot recovered: {}/{}",
        total,
        tps,
        tps as i64 - baseline_tps as i64,
        recover_count(&decoded),
        mid_slot.len()
    );

    // === Pre-emphasis filter (+3 dB above 1500 Hz) ===
    println!("\n### Pre-emphasis: +3 dB above 1500 Hz (single-pole)");
    eprintln!("  pre-emphasis…");
    let (decoded, total, tps) = run_pass(&entries, baseline_cfg.clone(), |s| {
        // Simple +3 dB shelf at 1500 Hz via 1st-order high-shelf approximation:
        //   y[n] = x[n] + 0.41 * (x[n] - x[n-1])
        // The 0.41 factor gives roughly +3 dB at ~3 kHz, less at 1500 Hz; this
        // is an experimental probe, not a precision EQ.
        let mut prev = 0.0f32;
        for x in s.iter_mut() {
            let y = *x + 0.41 * (*x - prev);
            prev = *x;
            *x = y;
        }
    })?;
    println!(
        "  pre-emphasis: {} decodes / {} TPs (Δ {:+} TPs); mid-slot recovered: {}/{}",
        total,
        tps,
        tps as i64 - baseline_tps as i64,
        recover_count(&decoded),
        mid_slot.len()
    );

    // === Combined: dither + DC-remove + osd=3 (cherry pick if individuals shipped) ===
    println!("\n### Combo: dither(0.005) + dc-remove + osd_depth=3");
    eprintln!("  combo…");
    let mut combo_cfg = baseline_cfg.clone();
    combo_cfg.osd_depth = Some(3);
    let (decoded, total, tps) = run_pass(&entries, combo_cfg, |s| {
        let mean = s.iter().copied().sum::<f32>() / s.len() as f32;
        let mut rng = rand::thread_rng();
        for x in s.iter_mut() {
            *x -= mean;
            let g: f32 = rng.gen_range(-1.0..1.0);
            *x += g * 0.005;
        }
    })?;
    println!(
        "  combo: {} decodes / {} TPs (Δ {:+} TPs); mid-slot recovered: {}/{}",
        total,
        tps,
        tps as i64 - baseline_tps as i64,
        recover_count(&decoded),
        mid_slot.len()
    );

    Ok(())
}
