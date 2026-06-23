//! Batch 28 / Diagnostic A — Config-permutation decode-set diversity
//!
//! Tests three hypotheses on the same decode pass:
//!
//! * **hb-040** (time_range dead code): vary `Ft8Config::time_range` ∈
//!   {1.0, 2.0, 4.0}. Expected: identical decode sets (confirms hb-025
//!   shelve finding that time_range is not threaded into the spectrogram).
//! * **hb-096** (adaptive multipass termination): vary
//!   `max_decode_passes` ∈ {1, 2}. Expected on production-config corpus
//!   (which already shipped max_passes=1 via hb-031): pass-2 finds 0 new
//!   decodes per WAV → confirms multipass is structurally dead and
//!   "adaptive termination" has no surface area.
//! * **hb-122** (sync-window LLR diversity via config proxy): vary
//!   `max_sync_candidates` ∈ {200, 300, 400} and `ldpc_iterations` ∈
//!   {50, 100, 200}. Take per-WAV decode-set UNION across all
//!   permutations. PROCEED if union exceeds the best individual config
//!   by ≥ 5% on majority of WAVs (indicates real diversity).
//!
//! Method: top-20 hard-200 WAVs, decode under each config, collect
//! text-set, intersect with jt9 truth, report.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch28_config_diversity

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

fn decode_set(samples: &[f32], cfg: &Ft8Config) -> Result<HashSet<String>> {
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(decoded.into_iter().map(|d| d.text).collect())
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH28_TOP_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    println!("## Batch 28 / Diagnostic A — config-permutation diversity (top-{top_n} hard-200)");

    // Configs to test. Group by hypothesis.
    let base = Ft8Config::default();

    let time_range_configs: Vec<(String, Ft8Config)> = [1.0_f64, 2.0, 4.0]
        .iter()
        .map(|&tr| {
            let mut c = base.clone();
            c.time_range = tr;
            (format!("tr={tr}"), c)
        })
        .collect();

    let multipass_configs: Vec<(String, Ft8Config)> = [1usize, 2]
        .iter()
        .map(|&n| {
            let mut c = base.clone();
            c.max_decode_passes = n;
            (format!("passes={n}"), c)
        })
        .collect();

    let sync_configs: Vec<(String, Ft8Config)> = [200usize, 300, 400]
        .iter()
        .map(|&n| {
            let mut c = base.clone();
            c.max_sync_candidates = n;
            (format!("sync_cap={n}"), c)
        })
        .collect();

    let ldpc_configs: Vec<(String, Ft8Config)> = [50usize, 100, 200]
        .iter()
        .map(|&n| {
            let mut c = base.clone();
            c.ldpc_iterations = n;
            (format!("ldpc={n}"), c)
        })
        .collect();

    // -- hb-040: time_range invariance --
    let mut tr_wav_results: Vec<(String, Vec<usize>)> = Vec::new();
    // -- hb-096: multipass increment --
    let mut mp_increments: Vec<i64> = Vec::new();
    let mut mp_tp_increments: Vec<i64> = Vec::new();
    // -- hb-122: sync × ldpc union vs max individual --
    let mut union_lifts: Vec<f64> = Vec::new();
    let mut sync_individual_max: Vec<usize> = Vec::new();
    let mut sync_union_size: Vec<usize> = Vec::new();
    // hb-122 truth-intersected: same but counting TPs only.
    let mut tp_union_lifts: Vec<f64> = Vec::new();
    let mut tp_individual_max: Vec<usize> = Vec::new();
    let mut tp_union_size: Vec<usize> = Vec::new();

    let total_tr_decoded_sets_match = std::cell::RefCell::new(0usize);
    let total_tr_decoded_sets_total = std::cell::RefCell::new(0usize);

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let _ = &truth; // for future use

        // hb-040: time_range sweep
        let tr_sets: Vec<HashSet<String>> = time_range_configs
            .iter()
            .map(|(_, c)| decode_set(&samples, c))
            .collect::<Result<Vec<_>>>()?;
        let tr_baseline = &tr_sets[1]; // tr=2.0
        let mut tr_match_count = 0;
        for s in &tr_sets {
            if s == tr_baseline {
                tr_match_count += 1;
            }
        }
        *total_tr_decoded_sets_match.borrow_mut() += tr_match_count;
        *total_tr_decoded_sets_total.borrow_mut() += tr_sets.len();
        tr_wav_results.push((
            sha[..8].to_string(),
            tr_sets.iter().map(|s| s.len()).collect(),
        ));

        // hb-096: multipass=2 increment over multipass=1
        let mp_sets: Vec<HashSet<String>> = multipass_configs
            .iter()
            .map(|(_, c)| decode_set(&samples, c))
            .collect::<Result<Vec<_>>>()?;
        let inc = mp_sets[1].difference(&mp_sets[0]).count() as i64
            - mp_sets[0].difference(&mp_sets[1]).count() as i64;
        mp_increments.push(inc);
        // Truth-intersected: did pass-2 add TPs that pass-1 missed?
        let mp_tp_sets: Vec<HashSet<String>> = mp_sets
            .iter()
            .map(|s| s.intersection(&truth).cloned().collect())
            .collect();
        let tp_inc = mp_tp_sets[1].difference(&mp_tp_sets[0]).count() as i64
            - mp_tp_sets[0].difference(&mp_tp_sets[1]).count() as i64;
        mp_tp_increments.push(tp_inc);

        // hb-122: sync × ldpc full union vs individual max
        let mut all_sync_sets: Vec<HashSet<String>> = Vec::new();
        for (_, sc) in &sync_configs {
            for (_, lc) in &ldpc_configs {
                let mut c = sc.clone();
                c.ldpc_iterations = lc.ldpc_iterations;
                let s = decode_set(&samples, &c)?;
                all_sync_sets.push(s);
            }
        }
        let max_individual = all_sync_sets.iter().map(|s| s.len()).max().unwrap_or(0);
        let mut union: HashSet<String> = HashSet::new();
        for s in &all_sync_sets {
            union.extend(s.iter().cloned());
        }
        let union_size = union.len();
        sync_individual_max.push(max_individual);
        sync_union_size.push(union_size);
        let lift = if max_individual == 0 {
            0.0
        } else {
            (union_size as f64 - max_individual as f64) / max_individual as f64
        };
        union_lifts.push(lift);

        // Truth-intersected variant: filter every set to ∩ truth before
        // computing union and max-individual. This is the metric that
        // matters — config diversity that just produces FP variation is
        // not LLR-averaging-worthy.
        let tp_sets: Vec<HashSet<String>> = all_sync_sets
            .iter()
            .map(|s| s.intersection(&truth).cloned().collect())
            .collect();
        let tp_max = tp_sets.iter().map(|s| s.len()).max().unwrap_or(0);
        let mut tp_union: HashSet<String> = HashSet::new();
        for s in &tp_sets {
            tp_union.extend(s.iter().cloned());
        }
        tp_individual_max.push(tp_max);
        tp_union_size.push(tp_union.len());
        let tp_lift = if tp_max == 0 {
            0.0
        } else {
            (tp_union.len() as f64 - tp_max as f64) / tp_max as f64
        };
        tp_union_lifts.push(tp_lift);
    }

    // -- hb-040 report --
    println!("\n### hb-040 — time_range invariance");
    println!(
        "  {:>10}  {:>10}  {:>10}  {:>10}",
        "WAV", "tr=1", "tr=2", "tr=4"
    );
    for (sha, counts) in &tr_wav_results {
        println!(
            "  {:>10}  {:>10}  {:>10}  {:>10}",
            sha, counts[0], counts[1], counts[2]
        );
    }
    let match_rate = *total_tr_decoded_sets_match.borrow() as f64
        / (*total_tr_decoded_sets_total.borrow()).max(1) as f64;
    println!(
        "  → {} / {} decode sets match tr=2 baseline ({:.1}%)",
        total_tr_decoded_sets_match.borrow(),
        total_tr_decoded_sets_total.borrow(),
        match_rate * 100.0
    );
    if match_rate > 0.99 {
        println!("  Verdict: SHELVE — time_range is dead config (confirms hb-040 static finding)");
    } else {
        println!("  Verdict: REQUIRES-FOLLOWUP — time_range produces decode-set variation");
    }

    // -- hb-096 report --
    println!("\n### hb-096 — multipass=2 increment over =1 (in production-config corpus)");
    let total_inc: i64 = mp_increments.iter().sum();
    let mean_inc = total_inc as f64 / mp_increments.len() as f64;
    let pos_count = mp_increments.iter().filter(|&&i| i > 0).count();
    let tp_total: i64 = mp_tp_increments.iter().sum();
    let tp_mean = tp_total as f64 / mp_tp_increments.len() as f64;
    let tp_pos_count = mp_tp_increments.iter().filter(|&&i| i > 0).count();
    println!(
        "  Raw decode counts: total net increment {} across {} WAVs (mean {:.2}/WAV; {} pos)",
        total_inc,
        mp_increments.len(),
        mean_inc,
        pos_count
    );
    println!(
        "  Truth-matched:     total net TP increment {} (mean {:.2}/WAV; {} pos)",
        tp_total, tp_mean, tp_pos_count
    );
    if tp_total.abs() <= 1 {
        println!("  Verdict: SHELVE — multipass-2 adds zero net TPs (raw +{} is all FPs); confirms hb-031 / production max_passes=1 is correct", total_inc);
    } else if tp_pos_count >= 3 {
        println!("  Verdict: PROCEED — multipass-2 adds real TPs ({} net); adaptive termination diagnostic worth next session", tp_total);
    } else {
        println!("  Verdict: NOISE — marginal TP differences");
    }

    // -- hb-122 report --
    println!("\n### hb-122 — sync×ldpc config diversity (decode-count proxy for LLR-diversity)");
    let mean_lift = union_lifts.iter().sum::<f64>() / union_lifts.len() as f64;
    let strong_count = union_lifts.iter().filter(|&&l| l >= 0.05).count();
    let tp_mean_lift = tp_union_lifts.iter().sum::<f64>() / tp_union_lifts.len() as f64;
    let tp_strong_count = tp_union_lifts.iter().filter(|&&l| l >= 0.05).count();
    println!(
        "  Raw decode counts:  mean union-vs-max lift: {:+.3}%  ({} of {} WAVs ≥ +5%)",
        mean_lift * 100.0,
        strong_count,
        union_lifts.len()
    );
    println!(
        "  Truth-matched only: mean union-vs-max lift: {:+.3}%  ({} of {} WAVs ≥ +5%)",
        tp_mean_lift * 100.0,
        tp_strong_count,
        tp_union_lifts.len()
    );
    if tp_strong_count * 2 >= tp_union_lifts.len() {
        println!("  Verdict: PROCEED — real TP diversity across sync/ldpc configs; LLR averaging worth a plan");
    } else if strong_count * 2 >= union_lifts.len() {
        println!("  Verdict: SHELVE — config diversity exists in TOTAL decodes but TP diversity does not; the +{:.0}% raw lift is FP variation across configs, not new TPs", mean_lift * 100.0);
    } else {
        println!("  Verdict: SHELVE — sync×ldpc configs produce essentially the same decode set");
    }

    Ok(())
}
