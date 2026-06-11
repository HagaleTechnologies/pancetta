//! Batch 70 — re-run the hb-103 v2 AUC measurement with ft8_lib truth.
//!
//! Batch 64 measured hb-103 v2 (the FDR-ConfidenceFeatures-extended
//! content score) against pancetta truth on hard_200 and found
//! ΔAUC = +0.0000 — v2 = v1.
//!
//! Per Batch 68-69 findings, pancetta-truth can hide signal that
//! ft8_lib truth surfaces. This probe re-runs the v1-vs-v2 comparison
//! with ft8_lib truth.
//!
//! Decision rule:
//!   - If v2 AUC > v1 AUC + 0.005 (smallest meaningful lift) →
//!     reconsider v2 as the new default content score
//!   - Otherwise: v1 stays
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch70_content_score_v2_ft8lib

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use pancetta_qso::callsign_continuity::CallsignContinuityFilter;
use pancetta_qso::content_score::{
    content_score_from_features, content_score_v2_from_features, ContentFeatures,
};
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

fn load_ft8lib_truth(ws: &Path, sha: &str) -> HashSet<String> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{sha}.ft8lib.json"));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return HashSet::new();
    };
    v["decodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
        .collect()
}

fn auc(tp: &[f64], fp: &[f64]) -> f64 {
    if tp.is_empty() || fp.is_empty() {
        return 0.5;
    }
    let mut wins = 0.0;
    let mut total = 0.0;
    for &t in tp {
        for &f in fp {
            total += 1.0;
            if t > f {
                wins += 1.0;
            } else if (t - f).abs() < 1e-12 {
                wins += 0.5;
            }
        }
    }
    wins / total
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
    println!("loaded hard_200: {} entries (ft8_lib truth)", entries.len());

    let cfg = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        ..Ft8Config::default()
    };

    // Seed trust filter with all ft8_lib-truth callsigns.
    let mut trust = CallsignContinuityFilter::new(2000);
    let mut all_calls: HashSet<String> = HashSet::new();
    for entry in &entries {
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        for t in load_ft8lib_truth(&ws, sha) {
            for c in pancetta_qso::callsign_continuity::callsigns_in(&t) {
                all_calls.insert(c);
            }
        }
    }
    trust.extend_from_iter(all_calls.iter());
    println!(
        "trust filter: {} callsigns from ft8_lib truth",
        all_calls.len()
    );

    let mut v1_tp: Vec<f64> = Vec::new();
    let mut v1_fp: Vec<f64> = Vec::new();
    let mut v2_tp: Vec<f64> = Vec::new();
    let mut v2_fp: Vec<f64> = Vec::new();

    for (i, entry) in entries.iter().enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_ft8lib_truth(&ws, sha);

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        for d in &decoded {
            let cf = d.confidence_features.as_ref();
            let feat = ContentFeatures {
                text: &d.text,
                confidence: d.confidence,
                snr_db: d.snr_db,
                time_offset: d.time_offset,
                bp_iterations_used: cf.and_then(|c| c.bp_iterations_used),
                osd_depth_used: cf.and_then(|c| c.osd_depth_used),
                nharderrs: cf.and_then(|c| c.nharderrs),
                min_llr_magnitude: cf.and_then(|c| c.min_llr_magnitude),
            };
            let s1 = content_score_from_features(feat, &trust);
            let s2 = content_score_v2_from_features(feat, &trust);
            let is_tp = truth.contains(&d.text);
            if is_tp {
                v1_tp.push(s1);
                v2_tp.push(s2);
            } else {
                v1_fp.push(s1);
                v2_fp.push(s2);
            }
        }
        if (i + 1) % 50 == 0 {
            eprintln!("    [{}/{}]", i + 1, entries.len());
        }
    }

    let v1_auc = auc(&v1_tp, &v1_fp);
    let v2_auc = auc(&v2_tp, &v2_fp);
    let delta = v2_auc - v1_auc;
    println!(
        "\n| Formula | AUC | TPs | FPs |\n|---|---:|---:|---:|\n\
         | v1 (Batch 31) | {v1_auc:.4} | {} | {} |\n\
         | v2 (Batch 64) | {v2_auc:.4} | {} | {} |\n\n\
         ΔAUC: {delta:+.4}\n",
        v1_tp.len(),
        v1_fp.len(),
        v2_tp.len(),
        v2_fp.len(),
    );

    let decision = if delta >= 0.005 {
        format!("**Ship v2**: ΔAUC = {delta:+.4} ≥ +0.005 lift threshold against ft8_lib truth. Reconsider v2 as the autonomous-TX default.")
    } else if delta > 0.0 {
        format!("**Marginal under ft8_lib truth**: ΔAUC = {delta:+.4}. v1 stays default; v2 still available opt-in.")
    } else if delta.abs() < 0.001 {
        "**v2 still no-op under ft8_lib truth**: ConfidenceFeatures don't add discrimination on this corpus + this truth. Same verdict as Batch 64.".to_string()
    } else {
        format!("**v2 regresses under ft8_lib truth**: ΔAUC = {delta:+.4}. Keep v1.")
    };
    println!("## Decision\n\n{decision}\n");

    let notes_path = ws.join("research/notes/2026-06-09-batch70-v2-ft8lib.md");
    let body = format!(
        "# Batch 70 — hb-103 v2 AUC under ft8_lib truth (hard_200)\n\n\
         | Formula | AUC | TPs | FPs |\n|---|---:|---:|---:|\n\
         | v1 | {v1_auc:.4} | {} | {} |\n\
         | v2 | {v2_auc:.4} | {} | {} |\n\nΔAUC: {delta:+.4}\n\n{decision}\n",
        v1_tp.len(),
        v1_fp.len(),
        v2_tp.len(),
        v2_fp.len(),
    );
    std::fs::write(&notes_path, body)?;
    Ok(())
}
