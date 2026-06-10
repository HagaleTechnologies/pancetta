//! Batch 64 — hb-103 v2 AUC measurement on hard-200.
//!
//! Compares the v1 fused content score (Batch 31 formula) against the
//! v2 formula (Batch 64) which extends v1 with FDR ConfidenceFeatures
//! telemetry. The decision: does the additional telemetry improve
//! TP-vs-FP discrimination (Mann-Whitney AUC)?
//!
//! For each decoded message produced on hard-200:
//!   - Compute v1 score
//!   - Compute v2 score
//!   - Check if text matches truth (TP / FP label)
//!
//! Compute Mann-Whitney AUC for both score series. Higher AUC = better
//! discrimination. Decision rule:
//!   - If v2_AUC > v1_AUC + 0.005 (smallest meaningful lift) → ship v2
//!     as the new default content score in autonomous-TX gating.
//!   - Otherwise: keep v1 as the gated path; v2 is opt-in for any
//!     consumer that wants to experiment.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch64_content_score_v2_auc

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
        .into_iter()
        .flatten()
        .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
        .collect()
}

/// Mann-Whitney U as an AUC. AUC = P(score(TP) > score(FP)).
fn auc(tp_scores: &[f64], fp_scores: &[f64]) -> f64 {
    if tp_scores.is_empty() || fp_scores.is_empty() {
        return 0.5;
    }
    let mut wins = 0.0_f64;
    let mut total = 0.0_f64;
    for &tp in tp_scores {
        for &fp in fp_scores {
            total += 1.0;
            if tp > fp {
                wins += 1.0;
            } else if (tp - fp).abs() < 1e-12 {
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
    println!("loaded hard-200: {} entries", entries.len());

    let cfg = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        ..Ft8Config::default()
    };

    // Build a trust filter populated with the truth callsigns aggregated
    // across hard-200. This simulates "operator has worked this corpus's
    // callsigns before" — the production filter is populated from
    // ADIF + cqdx; for measurement we use truth as a stand-in.
    let mut trust = CallsignContinuityFilter::new(2000);
    let mut all_truth_calls: HashSet<String> = HashSet::new();
    for entry in &entries {
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        for t in load_truth(&ws, sha) {
            for c in pancetta_qso::callsign_continuity::callsigns_in(&t) {
                all_truth_calls.insert(c);
            }
        }
    }
    trust.extend_from_iter(all_truth_calls.iter());
    println!(
        "trust filter seeded with {} unique callsigns",
        all_truth_calls.len()
    );

    let mut v1_tp: Vec<f64> = Vec::new();
    let mut v1_fp: Vec<f64> = Vec::new();
    let mut v2_tp: Vec<f64> = Vec::new();
    let mut v2_fp: Vec<f64> = Vec::new();

    let t0 = std::time::Instant::now();
    for (i, entry) in entries.iter().enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);

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
    let secs = t0.elapsed().as_secs_f64();

    let v1_auc = auc(&v1_tp, &v1_fp);
    let v2_auc = auc(&v2_tp, &v2_fp);
    let delta = v2_auc - v1_auc;

    println!(
        "\n## hb-103 AUC on hard-200\n\n\
         | Formula | AUC | TPs | FPs |\n|---|---:|---:|---:|\n\
         | v1 (Batch 31)   | {:.4} | {} | {} |\n\
         | v2 (Batch 64)   | {:.4} | {} | {} |\n\n\
         **ΔAUC**: {:+.4}\n",
        v1_auc,
        v1_tp.len(),
        v1_fp.len(),
        v2_auc,
        v2_tp.len(),
        v2_fp.len(),
        delta,
    );

    let decision = if delta >= 0.005 {
        format!(
            "**Ship v2**: ΔAUC = {delta:+.4} ≥ +0.005 lift threshold. v2 becomes the recommended formula for autonomous-TX gating; v1 retired."
        )
    } else if delta > 0.0 {
        format!(
            "**Marginal v2 improvement** ({delta:+.4}); below the +0.005 ship threshold. Keep v1 as gated default; v2 available as opt-in."
        )
    } else if delta.abs() < 0.001 {
        "**v2 is no-op on hard-200**: ConfidenceFeatures telemetry doesn't add discrimination signal beyond v1. Likely because hard-200's standard messages all converge fast at BP-direct (low BP iters, high min_llr, no OSD), so the telemetry doesn't help separate TPs from FPs. v1 stays as the default; v2 available for opt-in.".to_string()
    } else {
        format!(
            "**v2 regresses** ({delta:+.4}): the weighted-sum form picks up noise from missing telemetry on FFI / AP decodes. Re-tune weights or stay on v1."
        )
    };
    println!("## Decision\n\n{decision}\n\nElapsed: {secs:.1}s");

    let notes_path = ws.join("research/notes/2026-06-09-batch64-content-score-v2.md");
    let body = format!(
        "# Batch 64 — hb-103 v2 AUC measurement\n\n\
         | Formula | AUC | TPs | FPs |\n|---|---:|---:|---:|\n\
         | v1 (Batch 31) | {:.4} | {} | {} |\n\
         | v2 (Batch 64) | {:.4} | {} | {} |\n\n\
         ΔAUC: {:+.4}\n\n\
         {decision}\n\nElapsed: {secs:.1}s\n",
        v1_auc,
        v1_tp.len(),
        v1_fp.len(),
        v2_auc,
        v2_tp.len(),
        v2_fp.len(),
        delta,
    );
    std::fs::write(&notes_path, body)?;
    Ok(())
}
