//! Batch 80 — hb-103 v3 operating-point calibration for the autonomous
//! CQ gate.
//!
//! The production gate (autonomous.rs decide()) scores CQ candidates
//! with content score v1 and rejects below SHIP_CONSERVATIVE (+0.35)
//! (Diagnostic T: 100% TP recall, −34% FPs on hard-200). Batch 79
//! validated the v3 decode-lateness term (+0.032 / +0.012 held-out
//! ΔAUC). This probe asks the production question directly:
//!
//!   At a τ_v3 calibrated for 100% TP recall, does v3 reject MORE FPs
//!   than v1 @ +0.35 on the gate's population (CQ-type decodes)?
//!
//! Populations reported: all decodes (context) and CQ-only (decision).
//! Corpora: hard_200 + raw_530 subset-200, ft8_lib truth.
//!
//! Decision rule: v3 dominates (recall ≥ v1's AND FP rejection > v1's)
//! on BOTH corpora's CQ population → flip the gate to v3 with the
//! calibrated τ (take the more conservative τ of the two corpora).
//! Otherwise → keep v1, journal.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch80_v3_operating_point

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use pancetta_qso::callsign_continuity::{callsigns_in, CallsignContinuityFilter};
use pancetta_qso::content_score::{
    content_score_from_features, content_score_v3_from_features, ContentFeatures,
    MessageContentScore,
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

struct Row {
    is_tp: bool,
    is_cq: bool,
    v1: f64,
    v3: f64,
}

fn gate_stats(rows: &[&Row], tau_v1: f64, tau_v3: f64) -> (f64, f64, f64, f64) {
    let tps: Vec<&&Row> = rows.iter().filter(|r| r.is_tp).collect();
    let fps: Vec<&&Row> = rows.iter().filter(|r| !r.is_tp).collect();
    let v1_recall = tps.iter().filter(|r| r.v1 >= tau_v1).count() as f64 / tps.len().max(1) as f64;
    let v1_fp_rej = fps.iter().filter(|r| r.v1 < tau_v1).count() as f64 / fps.len().max(1) as f64;
    let v3_recall = tps.iter().filter(|r| r.v3 >= tau_v3).count() as f64 / tps.len().max(1) as f64;
    let v3_fp_rej = fps.iter().filter(|r| r.v3 < tau_v3).count() as f64 / fps.len().max(1) as f64;
    (v1_recall, v1_fp_rej, v3_recall, v3_fp_rej)
}

/// Largest τ that keeps 100% recall on the given rows' TPs.
fn tau_full_recall(rows: &[&Row]) -> f64 {
    rows.iter()
        .filter(|r| r.is_tp)
        .map(|r| r.v3)
        .fold(f64::INFINITY, f64::min)
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let mut body =
        String::from("# Batch 80 — hb-103 v3 operating point for the autonomous CQ gate\n\n");
    let mut corpora_rows: Vec<(String, Vec<Row>)> = Vec::new();

    for (label, manifest_name, take) in [
        ("hard_200", "hard_200.manifest.json", usize::MAX),
        ("raw_530 subset-200", "raw_530_full.manifest.json", 200usize),
    ] {
        let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
            ws.join("research/corpus/curated/ft8").join(manifest_name),
        )?)?;
        let entries: Vec<Value> = manifest["entries"]
            .as_array()
            .context("entries")?
            .iter()
            .take(take)
            .cloned()
            .collect();
        eprintln!("---- {label}: {} entries ----", entries.len());

        // Trust filter seeded from corpus truth callsigns (Batch 70 pattern).
        let mut trust = CallsignContinuityFilter::new(4000);
        let mut all_calls: HashSet<String> = HashSet::new();
        for entry in &entries {
            let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
            for t in load_ft8lib_truth(&ws, sha) {
                for c in callsigns_in(&t) {
                    all_calls.insert(c);
                }
            }
        }
        trust.extend_from_iter(all_calls.iter());

        let cfg = Ft8Config::default();
        let mut rows: Vec<Row> = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            let wav_path = entry["wav_path"].as_str().context("wav_path")?;
            let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
            let samples = match load_wav(Path::new(wav_path)) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let truth = load_ft8lib_truth(&ws, sha);
            let mut decoder = Ft8Decoder::new(cfg.clone())
                .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
            let decoded = decoder
                .decode_window(&samples)
                .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
            let slot_tmax = decoded
                .iter()
                .filter_map(|d| d.decode_time_into_window.map(|t| t.as_secs_f64()))
                .fold(1e-9f64, f64::max);
            for d in &decoded {
                let cf = d.confidence_features.as_ref();
                let frac = d
                    .decode_time_into_window
                    .map(|t| t.as_secs_f64() / slot_tmax);
                let mk = |frac: Option<f64>| ContentFeatures {
                    text: &d.text,
                    confidence: d.confidence,
                    snr_db: d.snr_db,
                    time_offset: d.time_offset,
                    bp_iterations_used: cf.and_then(|c| c.bp_iterations_used),
                    osd_depth_used: cf.and_then(|c| c.osd_depth_used),
                    nharderrs: cf.and_then(|c| c.nharderrs),
                    min_llr_magnitude: cf.and_then(|c| c.min_llr_magnitude),
                    decode_time_frac: frac,
                };
                // v1 gate mirrors production: no FDR telemetry on that path.
                let v1 = content_score_from_features(
                    ContentFeatures {
                        bp_iterations_used: None,
                        osd_depth_used: None,
                        nharderrs: None,
                        min_llr_magnitude: None,
                        decode_time_frac: None,
                        ..mk(None)
                    },
                    &trust,
                );
                let v3 = content_score_v3_from_features(mk(frac), &trust);
                rows.push(Row {
                    is_tp: truth.contains(&d.text),
                    is_cq: d.text.starts_with("CQ "),
                    v1,
                    v3,
                });
            }
            if (i + 1) % 50 == 0 {
                eprintln!("    [{label} {}/{}]", i + 1, entries.len());
            }
        }

        for (pop_label, pop) in [
            (
                "CQ-only (gate population)",
                rows.iter().filter(|r| r.is_cq).collect::<Vec<&Row>>(),
            ),
            ("all decodes", rows.iter().collect::<Vec<&Row>>()),
        ] {
            let tau3 = tau_full_recall(&pop);
            let (r1, f1, r3, f3) =
                gate_stats(&pop, MessageContentScore::SHIP_CONSERVATIVE, tau3);
            let n_tp = pop.iter().filter(|r| r.is_tp).count();
            let n_fp = pop.len() - n_tp;
            body.push_str(&format!(
                "## {label} — {pop_label}\n\n\
                 {n_tp} TPs / {n_fp} FPs\n\n\
                 | Gate | τ | TP recall | FP rejection |\n|---|---:|---:|---:|\n\
                 | v1 (production) | {:.3} | {:.4} | {:.4} |\n\
                 | v3 @ 100%-recall τ | {tau3:.3} | {r3:.4} | {f3:.4} |\n\n",
                MessageContentScore::SHIP_CONSERVATIVE,
                r1,
                f1,
            ));
            println!(
                "{label} / {pop_label}: v1 recall {r1:.4} fp_rej {f1:.4} | v3@τ={tau3:.3} recall {r3:.4} fp_rej {f3:.4}"
            );
        }
        corpora_rows.push((label.to_string(), rows));
    }

    // Cross-corpus-safe operating point: one fixed τ* = min over both
    // corpora's CQ-population 100%-recall τ. The gate ships one constant,
    // so this is the number the flip decision rides on.
    let tau_star = corpora_rows
        .iter()
        .map(|(_, rows)| tau_full_recall(&rows.iter().filter(|r| r.is_cq).collect::<Vec<&Row>>()))
        .fold(f64::INFINITY, f64::min);
    body.push_str(&format!(
        "## Cross-corpus-safe τ* = {tau_star:.3} (CQ population)\n\n         | Corpus | v1 FP rejection | v3 @ τ* recall | v3 @ τ* FP rejection |\n|---|---:|---:|---:|\n"
    ));
    for (label, rows) in &corpora_rows {
        let pop: Vec<&Row> = rows.iter().filter(|r| r.is_cq).collect();
        let (_, f1, r3, f3) = gate_stats(&pop, MessageContentScore::SHIP_CONSERVATIVE, tau_star);
        body.push_str(&format!("| {label} | {f1:.4} | {r3:.4} | {f3:.4} |\n"));
        println!("τ*={tau_star:.3} {label}: v3 recall {r3:.4} fp_rej {f3:.4} (v1 fp_rej {f1:.4})");
    }
    body.push('\n');

    let notes_path = ws.join("research/notes/2026-06-11-batch80-v3-operating-point.md");
    std::fs::write(&notes_path, &body)?;
    println!("wrote {}", notes_path.display());
    Ok(())
}
