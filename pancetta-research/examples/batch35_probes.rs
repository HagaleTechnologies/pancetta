//! Batch 35 / Items A + B + C — quick probes.
//!
//! A — slot-edge sync expansion: prepend 2s of silence to each WAV so
//!     pancetta's sync (which starts at t0=0) can pick up negative-dt
//!     signals. Compare recall delta vs original.
//!
//! B — compound-callsign emissions: count how many /R, /P, /M emissions
//!     pancetta produces vs how many /P, /R truths jt9 reports. The
//!     pancetta-ft8 renderer hardcodes /R for ip=1 — confirm pancetta
//!     emits zero /P and check if it consistently emits /R for what
//!     jt9 calls /P.
//!
//! C — multipass=2 on previously-missed positions: re-run hard-200 with
//!     max_decode_passes=2 and count TPs gained over default=1. Probes
//!     hb-218 (multipass infra) as a quick capture-effect recovery path.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch35_probes

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: usize = 12_000;

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

fn load_truth(ws: &Path, sha: &str) -> Vec<(String, f64)> {
    // (text, dt_s)
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return Vec::new();
    };
    let Some(arr) = v["decodes"].as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|d| Some((d["message"].as_str()?.to_string(), d["dt_s"].as_f64()?)))
        .collect()
}

fn count_suffix(text: &str, suffix: &str) -> bool {
    text.split_whitespace().any(|t| {
        let base = t.split('/').nth(1).unwrap_or("");
        base == suffix
    })
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH35_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    println!("## Batch 35 — A + B + C probes");

    let cfg = Ft8Config::default();
    let mut cfg_mp2 = cfg.clone();
    cfg_mp2.max_decode_passes = 2;

    let mut a_default_neg_dt_recovered = 0usize;
    let mut a_padded_neg_dt_recovered = 0usize;
    let mut a_neg_dt_total = 0usize;
    let mut a_padded_new_tps = 0usize; // tps padded gets that default misses
    let mut a_padded_lost_tps = 0usize; // tps default has that padded misses

    let mut b_slash_r_emit = 0usize;
    let mut b_slash_p_emit = 0usize;
    let mut b_slash_m_emit = 0usize;
    let mut b_slash_p_truth = 0usize;
    let mut b_slash_r_truth = 0usize;
    let mut b_slash_m_truth = 0usize;

    let mut c_mp1_total = 0usize;
    let mut c_mp1_tp = 0usize;
    let mut c_mp2_total = 0usize;
    let mut c_mp2_tp = 0usize;
    let mut c_mp2_new_tp = 0usize; // TPs MP2 gets that MP1 doesn't
    let mut c_mp2_lost_tp = 0usize;

    // Restrict A to first 50 WAVs (slow because we decode twice each).
    let a_limit = top_n.min(50);

    for (idx, entry) in entries.iter().take(top_n).enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let truth_set: HashSet<String> = truth.iter().map(|t| t.0.clone()).collect();

        for (text, _) in &truth {
            if count_suffix(text, "P") {
                b_slash_p_truth += 1;
            }
            if count_suffix(text, "R") {
                b_slash_r_truth += 1;
            }
            if count_suffix(text, "M") {
                b_slash_m_truth += 1;
            }
        }

        // Default (mp=1) decode — used by A, B, C
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let default_decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        let default_set: HashSet<String> = default_decoded.iter().map(|d| d.text.clone()).collect();

        // B — count compound-callsign emissions
        for d in &default_decoded {
            if count_suffix(&d.text, "P") {
                b_slash_p_emit += 1;
            }
            if count_suffix(&d.text, "R") {
                b_slash_r_emit += 1;
            }
            if count_suffix(&d.text, "M") {
                b_slash_m_emit += 1;
            }
        }

        // C — multipass=2 decode
        let mut decoder_mp2 = Ft8Decoder::new(cfg_mp2.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new(mp2): {e}"))?;
        let mp2_decoded = decoder_mp2
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window(mp2): {e}"))?;
        let mp2_set: HashSet<String> = mp2_decoded.iter().map(|d| d.text.clone()).collect();

        c_mp1_total += default_set.len();
        c_mp2_total += mp2_set.len();
        for t in &default_set {
            if truth_set.contains(t) {
                c_mp1_tp += 1;
            }
        }
        for t in &mp2_set {
            if truth_set.contains(t) {
                c_mp2_tp += 1;
                if !default_set.contains(t) {
                    c_mp2_new_tp += 1;
                }
            }
        }
        for t in &default_set {
            if truth_set.contains(t) && !mp2_set.contains(t) {
                c_mp2_lost_tp += 1;
            }
        }

        // A — slot-edge probe: pad 2s of silence at the start
        if idx < a_limit {
            let pad_samples = SAMPLE_RATE * 2;
            let mut padded = vec![0.0f32; pad_samples];
            padded.extend_from_slice(&samples);
            let mut dec_pad = Ft8Decoder::new(cfg.clone())
                .map_err(|e| anyhow::anyhow!("Ft8Decoder::new(pad): {e}"))?;
            let padded_decoded = dec_pad
                .decode_window(&padded)
                .map_err(|e| anyhow::anyhow!("decode_window(pad): {e}"))?;
            let padded_set: HashSet<String> =
                padded_decoded.iter().map(|d| d.text.clone()).collect();

            for (text, dt_s) in &truth {
                if *dt_s >= 0.0 {
                    continue;
                }
                a_neg_dt_total += 1;
                if default_set.contains(text) {
                    a_default_neg_dt_recovered += 1;
                }
                if padded_set.contains(text) {
                    a_padded_neg_dt_recovered += 1;
                }
            }
            // count new/lost TPs from padding (across all truths, not just neg-dt)
            for t in &padded_set {
                if truth_set.contains(t) && !default_set.contains(t) {
                    a_padded_new_tps += 1;
                }
            }
            for t in &default_set {
                if truth_set.contains(t) && !padded_set.contains(t) {
                    a_padded_lost_tps += 1;
                }
            }
        }
    }

    println!(
        "\n### A — slot-edge probe (2s silence pad, first {} WAVs)",
        a_limit
    );
    println!("  negative-dt truths examined: {}", a_neg_dt_total);
    println!(
        "  default-cfg recovered:       {} ({:.1}%)",
        a_default_neg_dt_recovered,
        a_default_neg_dt_recovered as f64 / a_neg_dt_total.max(1) as f64 * 100.0
    );
    println!(
        "  padded-cfg recovered:        {} ({:.1}%)",
        a_padded_neg_dt_recovered,
        a_padded_neg_dt_recovered as f64 / a_neg_dt_total.max(1) as f64 * 100.0
    );
    println!(
        "  net new TPs from padding:    {}  net lost TPs: {}",
        a_padded_new_tps, a_padded_lost_tps
    );

    println!("\n### B — compound-callsign emissions vs truths");
    println!(
        "  truth:    /P={}  /R={}  /M={}",
        b_slash_p_truth, b_slash_r_truth, b_slash_m_truth
    );
    println!(
        "  pancetta: /P={}  /R={}  /M={}",
        b_slash_p_emit, b_slash_r_emit, b_slash_m_emit
    );
    if b_slash_p_emit == 0 && b_slash_p_truth > 0 {
        println!("  → pancetta emits ZERO /P. Likely renders ip=1 as /R only, mismatching jt9's /P convention.");
    }

    println!("\n### C — multipass=2 vs default (full hard-200)");
    println!(
        "  default (mp=1): {} decodes, {} TPs",
        c_mp1_total, c_mp1_tp
    );
    println!(
        "  multipass=2:    {} decodes, {} TPs",
        c_mp2_total, c_mp2_tp
    );
    println!(
        "  net TP delta:  +{} (mp2 gains over mp1), -{} (mp1 wins not in mp2)",
        c_mp2_new_tp, c_mp2_lost_tp
    );
    let net = c_mp2_new_tp as i64 - c_mp2_lost_tp as i64;
    println!("  net:           {}", net);

    Ok(())
}
