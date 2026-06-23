//! hb-117 — AGC-diversity: re-decode at multiple synthetic gain settings — kill-switch
//!
//! Question: pancetta-ft8's float32 internal pipeline is *supposed* to be
//! gain-invariant. Test whether real-world quantization, log-domain
//! threshold, or SNR-estimator nonlinearity produces decode-set diversity
//! across three gain settings: -12 dB, 0 dB (baseline), +12 dB.
//!
//! Kill-switch (per bank entry hb-117):
//!   "20 hard-200, rescale ±12 dB, count NEW true-positives at off-baseline
//!    gains. < 1.0 per WAV mean → too weak to mine."
//!
//! PROCEED if mean(off-baseline true-positive gains, per WAV) ≥ 1.0.
//! SHELVE if < 1.0 — float32 pipeline is gain-invariant enough that off-
//! baseline runs reproduce the baseline decode set.
//!
//! Method:
//!   1. Load top-N (default 20) hard-200 WAVs.
//!   2. For each gain g ∈ {-12 dB, 0 dB, +12 dB} (linear factors 0.2512,
//!      1.0, 3.9811):
//!        - rescale samples
//!        - decode with `Ft8Config::default()`
//!   3. Truth = jt9 baseline messages for the WAV.
//!   4. For each off-baseline gain (-12 or +12):
//!        - "new TP" = decoded(g) ∩ truth   \   decoded(0 dB)
//!        - count
//!   5. Report per-WAV breakdown + mean across 20 WAVs.
//!
//! Edge note: hb-069 (dB-vs-linear shelve) found log-domain matters
//! somewhere in pancetta's pipeline. If hb-117 shelves cleanly here, that
//! "log-domain matters" effect is small enough to not produce decode-set
//! diversity at ±12 dB on hard-200.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb117_agc_diversity_diagnostic
//!
//! Tunable: HB117_TOP_N (default 20), HB117_GAINS_DB (default "-12,0,12").
//!
//! Output: per-WAV table + verdict.

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
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
    anyhow::ensure!(
        spec.channels == 1 && spec.sample_rate == 12000,
        "expected 12 kHz mono"
    );
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    })
}

fn load_truth_messages(ws: &Path, sha: &str) -> HashSet<String> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return HashSet::new();
    };
    let Some(arr) = v["decodes"].as_array() else {
        return HashSet::new();
    };
    arr.iter()
        .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
        .collect()
}

fn decode_texts(samples: &[f32], cfg: &Ft8Config) -> Result<HashSet<String>> {
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(decoded.into_iter().map(|d| d.text).collect())
}

fn db_to_linear(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

fn rescaled(samples: &[f32], gain_linear: f32) -> Vec<f32> {
    samples.iter().map(|s| s * gain_linear).collect()
}

#[derive(Debug, Default, Clone)]
struct WavRow {
    sha8: String,
    truth_count: usize,
    /// per-gain (decoded count, decoded ∩ truth count)
    per_gain: Vec<(usize, usize)>,
    /// NEW true-positives at off-baseline gains, summed across off-baseline.
    novel_tp_off_baseline: usize,
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("no entries")?;

    let top_n: usize = std::env::var("HB117_TOP_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let gains_db: Vec<f32> = std::env::var("HB117_GAINS_DB")
        .unwrap_or_else(|_| "-12,0,12".to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    anyhow::ensure!(!gains_db.is_empty(), "no gain levels");
    let baseline_idx = gains_db
        .iter()
        .position(|&g| g == 0.0)
        .context("gains list must include 0 dB as baseline")?;

    println!(
        "## hb-117 AGC-diversity diagnostic — top-{} hard-200 WAVs, gains {:?} dB",
        top_n, gains_db
    );

    let cfg = Ft8Config::default();
    let mut rows: Vec<WavRow> = Vec::new();

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let original = load_wav(Path::new(wav_path))?;
        let truth = load_truth_messages(&ws, sha);

        let mut per_gain: Vec<(usize, usize)> = Vec::with_capacity(gains_db.len());
        let mut decoded_sets: Vec<HashSet<String>> = Vec::with_capacity(gains_db.len());

        for &g_db in &gains_db {
            let gain = db_to_linear(g_db);
            let samples = if g_db == 0.0 {
                original.clone()
            } else {
                rescaled(&original, gain)
            };
            let set = decode_texts(&samples, &cfg)?;
            let tp = set.intersection(&truth).count();
            per_gain.push((set.len(), tp));
            decoded_sets.push(set);
        }

        let baseline_set = &decoded_sets[baseline_idx];
        let mut novel_tp_off_baseline = 0usize;
        for (i, set) in decoded_sets.iter().enumerate() {
            if i == baseline_idx {
                continue;
            }
            let novel: HashSet<&String> = set.difference(baseline_set).collect();
            let novel_tp = novel.into_iter().filter(|s| truth.contains(*s)).count();
            novel_tp_off_baseline += novel_tp;
        }

        rows.push(WavRow {
            sha8: sha[..8].to_string(),
            truth_count: truth.len(),
            per_gain,
            novel_tp_off_baseline,
        });
    }

    println!(
        "\n  WAV          truth  {} novel_TP_off",
        gains_db
            .iter()
            .map(|g| format!("{:+3}dB(tot/tp)", *g as i32))
            .collect::<Vec<_>>()
            .join("  "),
    );
    println!(
        "  ----------   -----  {}  ------------",
        gains_db
            .iter()
            .map(|_| "------------")
            .collect::<Vec<_>>()
            .join("  "),
    );
    for r in &rows {
        let per_gain_str = r
            .per_gain
            .iter()
            .map(|(tot, tp)| format!("    {:>3}/{:>3}  ", tot, tp))
            .collect::<Vec<_>>()
            .join("");
        println!(
            "  {:8}      {:>3}{}    {:>4}",
            r.sha8, r.truth_count, per_gain_str, r.novel_tp_off_baseline,
        );
    }

    let total_novel: usize = rows.iter().map(|r| r.novel_tp_off_baseline).sum();
    let total_truth: usize = rows.iter().map(|r| r.truth_count).sum();
    let mean_novel_per_wav = total_novel as f64 / rows.len().max(1) as f64;

    println!();
    println!("## Summary");
    println!("  WAVs scored:                          {}", rows.len());
    println!("  Total truth across WAVs:              {}", total_truth);
    println!("  Total novel TP at off-baseline gains: {}", total_novel);
    println!(
        "  Mean novel TP per WAV (off-baseline): {:.3}",
        mean_novel_per_wav
    );

    println!();
    if mean_novel_per_wav >= 1.0 {
        println!(
            "## Verdict: PROCEED  (mean {:.2}/WAV ≥ 1.0 threshold)",
            mean_novel_per_wav
        );
        println!("    Pancetta's float32 pipeline is NOT effectively gain-invariant; off-");
        println!("    baseline AGC re-decodes mine meaningful additional TPs. Next session:");
        println!("    plan-sized 3-gain ensemble vote integration, calibrate FP rate.");
    } else {
        println!(
            "## Verdict: SHELVE   (mean {:.2}/WAV < 1.0 threshold)",
            mean_novel_per_wav
        );
        println!("    Off-baseline gains produce no meaningful new TPs; pancetta's float32");
        println!("    pipeline is gain-invariant enough that AGC-diversity is a dead lever");
        println!("    on hard-200.");
    }

    Ok(())
}
