//! Batch 61 — FDR calibration on hard-200.
//!
//! Runs the standard decode pipeline, captures each decode's
//! `ConfidenceFeatures` (FDR Sessions 1-3 telemetry), translates them
//! into `(MessageCategory, FdrFeatures)`, and evaluates the FDR gate
//! at all three levels: Off / Level1 / Level2.
//!
//! For each level, reports per-message-type:
//!   - decodes (total)
//!   - TPs (text matches truth)
//!   - FPs (text doesn't match truth)
//!   - FDR-rejected decodes (would have been dropped at this level)
//!   - of those rejected, how many were TPs (cost) vs FPs (benefit)
//!
//! Decision rule for Session 4b ship:
//!   - Level1: if benefit_FPs > 0.5 × cost_TPs (5x more FPs killed
//!     than TPs killed), ship default-ON at Level1
//!   - Level2: ditto for Level2 numbers
//!   - Otherwise: ship default-OFF
//!
//! Inspired by spec ref `spec-wsjtx-improved-fdr.md` §"Estimated
//! port effort" → "Calibration".
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch61_fdr_calibrate

use anyhow::{Context, Result};
use pancetta_ft8::{ConfidenceFeatures, Ft8Config, Ft8Decoder, MessageType};
use pancetta_qso::{fdr_should_reject, FdrFeatures, FdrLevel, MessageCategory};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
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

/// Translate from pancetta_ft8 types into pancetta_qso FDR primitives.
/// Same translation the coordinator will own when Session 4b ships
/// the wiring.
fn translate_category(t: MessageType) -> MessageCategory {
    match t {
        MessageType::Standard | MessageType::Extended => MessageCategory::Standard,
        MessageType::FreeText => MessageCategory::FreeText,
        MessageType::Telemetry => MessageCategory::Telemetry,
        MessageType::DXpedition => MessageCategory::DXpedition,
        MessageType::NonStdCall => MessageCategory::NonStdCall,
        MessageType::Contest | MessageType::FieldDay | MessageType::RTTYRoundup => {
            MessageCategory::Contest
        }
        MessageType::Unknown => MessageCategory::Unknown,
    }
}

fn translate_features(cf: &ConfidenceFeatures) -> FdrFeatures {
    FdrFeatures {
        bp_iterations_used: cf.bp_iterations_used,
        osd_depth_used: cf.osd_depth_used,
        nharderrs: cf.nharderrs,
        min_llr_magnitude: cf.min_llr_magnitude,
    }
}

#[derive(Default, Debug, Clone)]
struct LevelStats {
    decodes: usize,
    tps: usize,
    fps: usize,
    rejected_tp: usize,
    rejected_fp: usize,
}

impl LevelStats {
    fn record(&mut self, is_tp: bool, would_reject: bool) {
        self.decodes += 1;
        if is_tp {
            self.tps += 1;
            if would_reject {
                self.rejected_tp += 1;
            }
        } else {
            self.fps += 1;
            if would_reject {
                self.rejected_fp += 1;
            }
        }
    }
    fn surviving_tps(&self) -> usize {
        self.tps - self.rejected_tp
    }
    fn surviving_fps(&self) -> usize {
        self.fps - self.rejected_fp
    }
}

fn category_label(c: MessageCategory) -> &'static str {
    match c {
        MessageCategory::Standard => "Standard",
        MessageCategory::FreeText => "FreeText",
        MessageCategory::Telemetry => "Telemetry",
        MessageCategory::DXpedition => "DXpedition",
        MessageCategory::NonStdCall => "NonStdCall",
        MessageCategory::Contest => "Contest",
        MessageCategory::Unknown => "Unknown",
    }
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

    // Per-category, per-level stats.
    // stats[level_idx][category_idx] = LevelStats
    let mut stats: HashMap<(FdrLevel, MessageCategory), LevelStats> = HashMap::new();
    let categories = [
        MessageCategory::Standard,
        MessageCategory::FreeText,
        MessageCategory::Telemetry,
        MessageCategory::DXpedition,
        MessageCategory::NonStdCall,
        MessageCategory::Contest,
        MessageCategory::Unknown,
    ];
    for &c in &categories {
        for &lv in &[FdrLevel::Off, FdrLevel::Level1, FdrLevel::Level2] {
            stats.insert((lv, c), LevelStats::default());
        }
    }

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
            let cat = translate_category(d.message.message_type);
            let features = d.confidence_features.as_ref().map(translate_features);
            let is_tp = truth.contains(&d.text);
            for &lv in &[FdrLevel::Off, FdrLevel::Level1, FdrLevel::Level2] {
                let reject = fdr_should_reject(cat, features.as_ref(), lv, false);
                stats.get_mut(&(lv, cat)).unwrap().record(is_tp, reject);
            }
        }
        if (i + 1) % 50 == 0 {
            eprintln!("    [{}/{}]", i + 1, entries.len());
        }
    }
    let secs = t0.elapsed().as_secs_f64();

    // ---- Report ----
    println!("\n## Per-level totals (across all categories)\n");
    println!("| Level | Decodes | Surviving TPs | Surviving FPs | Rejected TPs | Rejected FPs |");
    println!("|---|---:|---:|---:|---:|---:|");
    for &lv in &[FdrLevel::Off, FdrLevel::Level1, FdrLevel::Level2] {
        let agg = stats.iter().filter(|((l, _c), _)| *l == lv).fold(
            LevelStats::default(),
            |mut acc, (_, s)| {
                acc.decodes += s.decodes;
                acc.tps += s.tps;
                acc.fps += s.fps;
                acc.rejected_tp += s.rejected_tp;
                acc.rejected_fp += s.rejected_fp;
                acc
            },
        );
        println!(
            "| {:?} | {} | {} | {} | {} | {} |",
            lv,
            agg.decodes,
            agg.surviving_tps(),
            agg.surviving_fps(),
            agg.rejected_tp,
            agg.rejected_fp,
        );
    }

    println!("\n## Per-category stats (Level1)\n");
    println!("| Category | Decodes | TPs | FPs | Rej TPs | Rej FPs | TP cost % | FP benefit % |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    for &c in &categories {
        if let Some(s) = stats.get(&(FdrLevel::Level1, c)) {
            let tp_cost = if s.tps > 0 {
                s.rejected_tp as f64 / s.tps as f64 * 100.0
            } else {
                0.0
            };
            let fp_benefit = if s.fps > 0 {
                s.rejected_fp as f64 / s.fps as f64 * 100.0
            } else {
                0.0
            };
            println!(
                "| {} | {} | {} | {} | {} | {} | {:.1}% | {:.1}% |",
                category_label(c),
                s.decodes,
                s.tps,
                s.fps,
                s.rejected_tp,
                s.rejected_fp,
                tp_cost,
                fp_benefit,
            );
        }
    }

    println!("\n## Per-category stats (Level2)\n");
    println!("| Category | Decodes | TPs | FPs | Rej TPs | Rej FPs | TP cost % | FP benefit % |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    for &c in &categories {
        if let Some(s) = stats.get(&(FdrLevel::Level2, c)) {
            let tp_cost = if s.tps > 0 {
                s.rejected_tp as f64 / s.tps as f64 * 100.0
            } else {
                0.0
            };
            let fp_benefit = if s.fps > 0 {
                s.rejected_fp as f64 / s.fps as f64 * 100.0
            } else {
                0.0
            };
            println!(
                "| {} | {} | {} | {} | {} | {} | {:.1}% | {:.1}% |",
                category_label(c),
                s.decodes,
                s.tps,
                s.fps,
                s.rejected_tp,
                s.rejected_fp,
                tp_cost,
                fp_benefit,
            );
        }
    }

    // Decision logic per spec: ship default-ON if benefit_FPs > 0.5 * cost_TPs.
    let l1_agg = stats
        .iter()
        .filter(|((l, _c), _)| *l == FdrLevel::Level1)
        .fold(LevelStats::default(), |mut acc, (_, s)| {
            acc.rejected_tp += s.rejected_tp;
            acc.rejected_fp += s.rejected_fp;
            acc
        });
    let l2_agg = stats
        .iter()
        .filter(|((l, _c), _)| *l == FdrLevel::Level2)
        .fold(LevelStats::default(), |mut acc, (_, s)| {
            acc.rejected_tp += s.rejected_tp;
            acc.rejected_fp += s.rejected_fp;
            acc
        });

    let decision = if l1_agg.rejected_tp == 0 && l1_agg.rejected_fp > 0 {
        format!(
            "**Ship default-ON at Level1**: drops {} FPs with zero TP cost.",
            l1_agg.rejected_fp
        )
    } else if (l1_agg.rejected_fp as f64) > (l1_agg.rejected_tp as f64) * 2.0 {
        format!(
            "**Ship default-ON at Level1**: drops {} FPs vs {} TPs (>2x ratio).",
            l1_agg.rejected_fp, l1_agg.rejected_tp
        )
    } else if l2_agg.rejected_tp == 0 && l2_agg.rejected_fp > 0 {
        format!(
            "**Consider Level2 default-ON**: Level1 inert, Level2 drops {} FPs at zero TP cost.",
            l2_agg.rejected_fp
        )
    } else {
        format!(
            "**Ship default-OFF**: Level1 drops {} FPs / {} TPs ({:.2}x ratio); Level2 drops {} FPs / {} TPs. Either net-negative or insufficient FP-kill rate.",
            l1_agg.rejected_fp,
            l1_agg.rejected_tp,
            l1_agg.rejected_fp as f64 / l1_agg.rejected_tp.max(1) as f64,
            l2_agg.rejected_fp,
            l2_agg.rejected_tp,
        )
    };
    println!("\n## Decision\n\n{decision}\n");
    println!("Elapsed: {secs:.1}s");

    let notes_path = ws.join("research/notes/2026-06-09-batch61-fdr-calibrate.md");
    let body = format!(
        "# Batch 61 — FDR calibration on hard-200\n\n\
         {decision}\n\n\
         (See probe stdout for per-category Level1/Level2 stats; the\n\
         interesting metric is rejected_fp/rejected_tp ratio per category.)\n\n\
         Elapsed: {secs:.1}s\n"
    );
    std::fs::write(&notes_path, body)?;
    Ok(())
}
