//! Batch 81 — measure hb-247 decode_origin as the hb-103 v3 feature,
//! head-to-head against the wall-clock proxy it replaces.
//!
//! Batch 79 validated decode-lateness (wall-clock) at +0.032/+0.012
//! held-out ΔAUC; Batch 80 deferred the CQ-gate flip because the
//! 100%-recall τ moved 73% between runs (load-sensitive feature).
//! decode_origin is deterministic: same input → same feature → same τ.
//!
//! Measures on hard_200 + raw_530 subset-200 (ft8_lib truth):
//!   A. Split-half held-out ΔAUC of v2 + w·(origin/6) vs v2
//!      (same protocol as Batch 79), alongside the timing feature.
//!   B. CQ-population operating point: v1 @ SHIP_CONSERVATIVE vs
//!      origin-v3 @ cross-corpus 100%-recall τ* (Batch 80 protocol).
//!
//! Decision rule: origin-v3 ΔAUC within noise of timing-v3 or better,
//! AND dominates v1 at fixed τ* on both corpora → flip the autonomous
//! CQ gate to origin-v3 (production constant from this run is final —
//! the feature is run-stable by construction).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch81_origin_measure

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use pancetta_qso::callsign_continuity::{callsigns_in, CallsignContinuityFilter};
use pancetta_qso::content_score::{
    content_score_from_features, content_score_v2_from_features, ContentFeatures,
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

struct Sample {
    wav_idx: usize,
    is_tp: bool,
    is_cq: bool,
    v1: f64,
    v2: f64,
    origin_frac: f64, // decode_origin / 6, 0.0 when absent
    time_frac: f64,   // slot-max-normalized decode_time, 0.0 when absent
    origin_seen: bool,
}

fn auc(tp: &[f64], fp: &[f64]) -> f64 {
    if tp.is_empty() || fp.is_empty() {
        return f64::NAN;
    }
    let mut wins = 0.0f64;
    for t in tp {
        for f in fp {
            if t > f {
                wins += 1.0;
            } else if (t - f).abs() < f64::EPSILON {
                wins += 0.5;
            }
        }
    }
    wins / (tp.len() as f64 * fp.len() as f64)
}

fn auc_of(samples: &[&Sample], score: impl Fn(&Sample) -> f64) -> f64 {
    let tp: Vec<f64> = samples
        .iter()
        .filter(|s| s.is_tp)
        .map(|s| score(s))
        .collect();
    let fp: Vec<f64> = samples
        .iter()
        .filter(|s| !s.is_tp)
        .map(|s| score(s))
        .collect();
    auc(&tp, &fp)
}

fn split_half_delta(
    samples: &[Sample],
    feature: impl Fn(&Sample) -> f64 + Copy,
) -> (f64, Vec<f64>) {
    let grid = [-2.0f64, -1.0, -0.5, -0.25, -0.1, 0.0];
    let even: Vec<&Sample> = samples.iter().filter(|s| s.wav_idx % 2 == 0).collect();
    let odd: Vec<&Sample> = samples.iter().filter(|s| s.wav_idx % 2 == 1).collect();
    let mut deltas = Vec::new();
    let mut weights = Vec::new();
    for (train, test) in [(&even, &odd), (&odd, &even)] {
        let mut best = (0.0f64, f64::MIN);
        for &w in &grid {
            let a = auc_of(train, |s| s.v2 + w * feature(s));
            if a > best.1 {
                best = (w, a);
            }
        }
        weights.push(best.0);
        let v2_auc = auc_of(test, |s| s.v2);
        let v3_auc = auc_of(test, |s| s.v2 + best.0 * feature(s));
        deltas.push(v3_auc - v2_auc);
    }
    ((deltas[0] + deltas[1]) / 2.0, weights)
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let mut body = String::from(
        "# Batch 81 — decode_origin (hb-247) vs wall-clock lateness as the v3 feature\n\n",
    );
    let mut corpora: Vec<(String, Vec<Sample>)> = Vec::new();

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
        let mut samples: Vec<Sample> = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            let wav_path = entry["wav_path"].as_str().context("wav_path")?;
            let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
            let wav = match load_wav(Path::new(wav_path)) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let truth = load_ft8lib_truth(&ws, sha);
            let mut decoder = Ft8Decoder::new(cfg.clone())
                .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
            let decoded = decoder
                .decode_window(&wav)
                .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
            let slot_tmax = decoded
                .iter()
                .filter_map(|d| d.decode_time_into_window.map(|t| t.as_secs_f64()))
                .fold(1e-9f64, f64::max);
            for d in &decoded {
                let cf = d.confidence_features.as_ref();
                let origin = cf.and_then(|c| c.decode_origin);
                let feat = ContentFeatures {
                    text: &d.text,
                    confidence: d.confidence,
                    snr_db: d.snr_db,
                    time_offset: d.time_offset,
                    bp_iterations_used: cf.and_then(|c| c.bp_iterations_used),
                    osd_depth_used: cf.and_then(|c| c.osd_depth_used),
                    nharderrs: cf.and_then(|c| c.nharderrs),
                    min_llr_magnitude: cf.and_then(|c| c.min_llr_magnitude),
                    lateness_frac: None,
                };
                let v1_feat = ContentFeatures {
                    bp_iterations_used: None,
                    osd_depth_used: None,
                    nharderrs: None,
                    min_llr_magnitude: None,
                    ..feat
                };
                samples.push(Sample {
                    wav_idx: i,
                    is_tp: truth.contains(&d.text),
                    is_cq: d.text.starts_with("CQ "),
                    v1: content_score_from_features(v1_feat, &trust),
                    v2: content_score_v2_from_features(feat, &trust),
                    origin_frac: origin.map(|o| o as f64 / 6.0).unwrap_or(0.0),
                    time_frac: d
                        .decode_time_into_window
                        .map(|t| t.as_secs_f64() / slot_tmax)
                        .unwrap_or(0.0),
                    origin_seen: origin.is_some(),
                });
            }
            if (i + 1) % 50 == 0 {
                eprintln!("    [{label} {}/{}]", i + 1, entries.len());
            }
        }

        let n = samples.len();
        let stamped = samples.iter().filter(|s| s.origin_seen).count();
        let nonzero = samples.iter().filter(|s| s.origin_frac > 0.0).count();
        let (d_origin, w_o) = split_half_delta(&samples, |s| s.origin_frac);
        let (d_time, w_t) = split_half_delta(&samples, |s| s.time_frac);
        let all: Vec<&Sample> = samples.iter().collect();
        let solo_origin = auc_of(&all, |s| -s.origin_frac);
        body.push_str(&format!(
            "## {label}\n\n\
             {n} decodes, {stamped} origin-stamped ({nonzero} with origin > 0).\n\n\
             | Feature | mean held-out ΔAUC vs v2 | fold weights | solo AUC (inv) |\n|---|---:|---|---:|\n\
             | decode_origin/6 | {d_origin:+.4} | {w_o:?} | {solo_origin:.4} |\n\
             | decode_time slot-frac | {d_time:+.4} | {w_t:?} | — |\n\n"
        ));
        println!("{label}: origin ΔAUC {d_origin:+.4} (weights {w_o:?}) | time ΔAUC {d_time:+.4}");

        // Dump per-decode samples so operating-point analysis is
        // replayable without re-decoding the corpus.
        let stem = label.replace([' ', '-'], "_");
        let dump = ws.join(format!(
            "research/corpus/scans/batch81_samples_{stem}.jsonl"
        ));
        let mut out = String::new();
        for s in &samples {
            out.push_str(&format!(
                "{{\"wav_idx\":{},\"is_tp\":{},\"is_cq\":{},\"v1\":{:.6},\"v2\":{:.6},\"origin_frac\":{:.6},\"time_frac\":{:.6}}}\n",
                s.wav_idx, s.is_tp, s.is_cq, s.v1, s.v2, s.origin_frac, s.time_frac
            ));
        }
        std::fs::write(&dump, out)?;
        corpora.push((label.to_string(), samples));
    }

    // Operating point at cross-corpus τ* (origin feature, w = -1).
    let v3o = |s: &Sample| s.v2 - s.origin_frac;
    let tau_star = corpora
        .iter()
        .map(|(_, samples)| {
            samples
                .iter()
                .filter(|s| s.is_cq && s.is_tp)
                .map(v3o)
                .fold(f64::INFINITY, f64::min)
        })
        .fold(f64::INFINITY, f64::min);
    body.push_str(&format!(
        "## Operating points (CQ population, origin-v3 = v2 - origin/6, cross-corpus τ* = {tau_star:.3})\n\n\
         τ=0.35 keeps origin-0 decodes byte-identical to the v1 gate and only\n\
         tightens recovery-pass decodes; τ=0.90 / τ* raise the bar globally.\n\n\
         | Corpus | v1 @ 0.35 | origin-v3 @ 0.35 | origin-v3 @ 0.90 | origin-v3 @ τ* |\n|---|---|---|---|---|\n"
    ));
    for (label, samples) in &corpora {
        let pop: Vec<&Sample> = samples.iter().filter(|s| s.is_cq).collect();
        let tps: Vec<&&Sample> = pop.iter().filter(|s| s.is_tp).collect();
        let fps: Vec<&&Sample> = pop.iter().filter(|s| !s.is_tp).collect();
        let stat = |score: &dyn Fn(&Sample) -> f64, tau: f64| {
            let r = tps.iter().filter(|s| score(s) >= tau).count() as f64 / tps.len().max(1) as f64;
            let f = fps.iter().filter(|s| score(s) < tau).count() as f64 / fps.len().max(1) as f64;
            format!("{r:.4} / {f:.4}")
        };
        let v1s: &dyn Fn(&Sample) -> f64 = &|s: &Sample| s.v1;
        let v3s: &dyn Fn(&Sample) -> f64 = &v3o;
        body.push_str(&format!(
            "| {label} | {} | {} | {} | {} |\n",
            stat(v1s, MessageContentScore::SHIP_CONSERVATIVE),
            stat(v3s, MessageContentScore::SHIP_CONSERVATIVE),
            stat(v3s, 0.90),
            stat(v3s, tau_star),
        ));
        println!(
            "{label}: v1@0.35 {} | v3@0.35 {} | v3@0.90 {} | v3@τ* {}",
            stat(v1s, MessageContentScore::SHIP_CONSERVATIVE),
            stat(v3s, MessageContentScore::SHIP_CONSERVATIVE),
            stat(v3s, 0.90),
            stat(v3s, tau_star),
        );
    }

    let notes_path = ws.join("research/notes/2026-06-11-batch81-origin-measure.md");
    std::fs::write(&notes_path, &body)?;
    println!("wrote {}", notes_path.display());
    Ok(())
}
