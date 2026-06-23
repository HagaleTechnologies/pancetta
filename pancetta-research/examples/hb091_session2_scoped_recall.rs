//! hb-091 Session 2 — scoped-recall diagnostic (PROCEED/SHELVE gate).
//!
//! Session 1 (2026-06-02) measured the cost of TRUNCATING the slot to 14.0s
//! with a full Costas sweep: retention = 97.73% of recall(15.0s), Δ = −112
//! (CI [−141, −80]). That cleared the 95% PROCEED gate.
//!
//! Session 2 (this diagnostic) measures the cost of ALSO restricting the
//! Costas sweep to a narrow `freq_bin_range` around the in-QSO partner's
//! known frequency. The production a8 path will use scoping at t=13s in
//! addition to truncation, so the operationally meaningful question is:
//!
//!   Does scoped recall at 14.0s clear the 95% gate vs full recall at 15.0s?
//!
//! Method (per WAV in `research/corpus/curated/ft8/hard_200.manifest.json`):
//!
//!   1. recall(15.0s, full): standard decode of the full 15s buffer.
//!   2. recall(14.0s, full): truncate to 14.0s, full decode (Session 1 replication).
//!   3. recall(14.0s, scoped): for EACH jt9 truth, derive freq_bin from
//!      `truth.freq_hz`, run `decode_window_scoped(samples[..14.0s],
//!      (truth_bin - HALF_WIDTH)..=(truth_bin + HALF_WIDTH))`, and count
//!      THAT truth as recovered iff it appears in the scoped output.
//!
//! Bootstrap 95% CI on per-WAV deltas vs the 15.0s full-search baseline.
//!
//! PROCEED gate:
//!   - retention(scoped @ 14.0s) >= 95% of recall(full @ 15.0s), AND
//!   - bootstrap CI on Δ(scoped@14s − full@15s) excludes > 5% loss
//!     (CI_low > -0.05 * baseline_recovered).
//!
//! Scope half-width: ±2 bins ≈ ±12.5 Hz (the journal's "±10 Hz around
//! partner" rounded up to the bin grid). HALF_WIDTH = 2.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb091_session2_scoped_recall
//!
//! Smoke run (first 5 WAVs):
//!   cargo run --release -p pancetta-research --example hb091_session2_scoped_recall -- --max-wavs 5

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder, SAMPLE_RATE};
use pancetta_research::bootstrap_ci::bootstrap_recall_delta;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Default scope half-width in freq_bins (1 bin = 6.25 Hz). ±2 bins ≈ ±12.5 Hz.
/// Override with `--half-width N` on the CLI.
const DEFAULT_HALF_WIDTH: usize = 2;

/// Truncation for the partial-buffer scoped path (matches Session 1's
/// PROCEED cutoff).
const CUTOFF_S: f64 = 14.0;

/// Bootstrap-CI seed (reuses Session 1's seed convention).
const SEED: u64 = 091_2026_06_02;

fn workspace_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

fn load_wav_12k_mono(path: &Path) -> Result<Vec<f32>> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening WAV {}", path.display()))?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.channels == 1 && spec.sample_rate == 12000,
        "WAV {} not 12kHz mono (got {} ch, {} Hz)",
        path.display(),
        spec.channels,
        spec.sample_rate,
    );
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
    })
}

#[derive(Debug, Clone)]
struct Truth {
    message: String,
    freq_hz: f64,
}

/// Load jt9 baseline (message, freq_hz) tuples for a WAV by its sha256.
fn load_truths(ws: &Path, wav_sha256: &str) -> Result<Vec<Truth>> {
    let p = ws
        .join("research/baselines/ft8")
        .join(format!("{wav_sha256}.json"));
    if !p.exists() {
        return Ok(Vec::new());
    }
    let s = std::fs::read_to_string(&p)?;
    let v: Value = serde_json::from_str(&s)?;
    Ok(v.get("decodes")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|d| {
                    let msg = d.get("message").and_then(|m| m.as_str())?;
                    let freq = d.get("freq_hz").and_then(|f| f.as_f64())?;
                    Some(Truth {
                        message: msg.trim().to_string(),
                        freq_hz: freq,
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Count how many `truths` appear in `decoded` by exact-string match on `text.trim()`.
fn count_recovered(decoded: &[pancetta_ft8::DecodedMessage], truths: &[Truth]) -> u32 {
    let our: Vec<String> = decoded.iter().map(|m| m.text.trim().to_string()).collect();
    truths
        .iter()
        .filter(|t| our.iter().any(|d| d == &t.message))
        .count() as u32
}

/// Full-search recovery on a truncated buffer.
fn recovered_full(samples: &[f32], cutoff_samples: usize, truths: &[Truth]) -> u32 {
    let cut = cutoff_samples.min(samples.len());
    let buf = &samples[..cut];
    let mut decoder = match Ft8Decoder::new(Ft8Config::default()) {
        Ok(d) => d,
        Err(_) => return 0,
    };
    let decoded = match decoder.decode_window(buf) {
        Ok(d) => d,
        Err(_) => return 0,
    };
    count_recovered(&decoded, truths)
}

/// Scoped recovery on a truncated buffer.
///
/// For EACH truth, runs an independent scoped decode at that truth's
/// freq_bin ± `half_width` and checks whether the truth's message text
/// appears in the output. Models the production case where the
/// coordinator knows the in-QSO partner's freq exactly and scopes
/// decoding around it.
fn recovered_scoped(
    samples: &[f32],
    cutoff_samples: usize,
    truths: &[Truth],
    half_width: usize,
) -> u32 {
    let cut = cutoff_samples.min(samples.len());
    let buf = &samples[..cut];
    let tone_spacing_hz = 6.25;
    let mut recovered = 0u32;
    for truth in truths {
        let center = (truth.freq_hz / tone_spacing_hz).round() as usize;
        let lo = center.saturating_sub(half_width);
        let hi = center.saturating_add(half_width);
        // Independent decoder per truth — the freq_bin scope changes,
        // and the residual-multipass state should not leak across truths.
        let mut decoder = match Ft8Decoder::new(Ft8Config::default()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let decoded = match decoder.decode_window_scoped(buf, lo..=hi) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if decoded.iter().any(|m| m.text.trim() == truth.message) {
            recovered += 1;
        }
    }
    recovered
}

fn main() -> Result<()> {
    // ----- Args -----
    let mut max_wavs: Option<usize> = None;
    let mut half_width: usize = DEFAULT_HALF_WIDTH;
    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--max-wavs" => {
                max_wavs = Some(
                    iter.next()
                        .context("--max-wavs needs a value")?
                        .parse()
                        .context("--max-wavs not a number")?,
                );
            }
            "--half-width" => {
                half_width = iter
                    .next()
                    .context("--half-width needs a value")?
                    .parse()
                    .context("--half-width not a number")?;
            }
            other => anyhow::bail!("unknown arg: {other}"),
        }
    }

    let ws = workspace_root()?;
    let manifest_path = ws.join("research/corpus/curated/ft8/hard_200.manifest.json");
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    let entries: Vec<(String, PathBuf)> = manifest["entries"]
        .as_array()
        .context("manifest entries not array")?
        .iter()
        .map(|e| {
            let sha = e["wav_sha256"]
                .as_str()
                .context("missing wav_sha256")?
                .to_string();
            let path = PathBuf::from(
                e["wav_path"]
                    .as_str()
                    .context("missing wav_path")?
                    .to_string(),
            );
            Ok((sha, path))
        })
        .collect::<Result<_>>()?;
    let entries = if let Some(n) = max_wavs {
        entries.into_iter().take(n).collect::<Vec<_>>()
    } else {
        entries
    };
    let n_wavs = entries.len();

    println!("hb-091 Session 2 — scoped-recall diagnostic on hard-200");
    println!("  WAVs in tier:    {n_wavs}");
    println!("  Cutoff:          {CUTOFF_S}s");
    println!(
        "  Scope half-width: ±{half_width} bins (~±{:.1} Hz)",
        half_width as f64 * 6.25
    );
    println!();

    // Per-WAV vectors for bootstrap CI. Each tuple is (recovered, truth_count).
    let mut per_wav_full_15: Vec<(u32, u32)> = Vec::with_capacity(n_wavs);
    let mut per_wav_full_14: Vec<(u32, u32)> = Vec::with_capacity(n_wavs);
    let mut per_wav_scoped_14: Vec<(u32, u32)> = Vec::with_capacity(n_wavs);

    let mut sum_full_15 = 0u64;
    let mut sum_full_14 = 0u64;
    let mut sum_scoped_14 = 0u64;
    let mut sum_truths = 0u64;

    let cutoff_samples = (CUTOFF_S * SAMPLE_RATE as f64) as usize;
    let full_samples = (15.0 * SAMPLE_RATE as f64) as usize;

    let start = Instant::now();
    let mut last_progress = Instant::now();

    for (idx, (sha, wav_path)) in entries.iter().enumerate() {
        let samples = match load_wav_12k_mono(wav_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [{idx}] skip {wav_path:?}: {e}");
                per_wav_full_15.push((0, 0));
                per_wav_full_14.push((0, 0));
                per_wav_scoped_14.push((0, 0));
                continue;
            }
        };

        let truths = load_truths(&ws, sha)?;
        let truth_count = truths.len() as u32;
        sum_truths += truth_count as u64;

        let r_full_15 = recovered_full(&samples, full_samples, &truths);
        let r_full_14 = recovered_full(&samples, cutoff_samples, &truths);
        let r_scoped_14 = recovered_scoped(&samples, cutoff_samples, &truths, half_width);

        per_wav_full_15.push((r_full_15, truth_count));
        per_wav_full_14.push((r_full_14, truth_count));
        per_wav_scoped_14.push((r_scoped_14, truth_count));

        sum_full_15 += r_full_15 as u64;
        sum_full_14 += r_full_14 as u64;
        sum_scoped_14 += r_scoped_14 as u64;

        if last_progress.elapsed().as_secs() >= 30 {
            let done = idx + 1;
            let pct = 100.0 * done as f64 / n_wavs as f64;
            let elapsed_s = start.elapsed().as_secs_f64();
            let eta_s = if done > 0 {
                elapsed_s * (n_wavs - done) as f64 / done as f64
            } else {
                0.0
            };
            println!(
                "  progress: {done}/{n_wavs} ({pct:.1}%) elapsed={elapsed_s:.0}s eta={eta_s:.0}s | running full15={sum_full_15} full14={sum_full_14} scoped14={sum_scoped_14}"
            );
            last_progress = Instant::now();
        }
    }

    let elapsed_total = start.elapsed().as_secs_f64();
    println!("\nDecode pass complete in {elapsed_total:.1}s\n");

    // ----- Report: recovery curve -----
    println!("{:-<70}", "");
    println!(
        "{:>20} {:>10} {:>10} {:>10}",
        "arm", "recovered", "truths", "recall"
    );
    println!("{:-<70}", "");
    let recall = |sum: u64| {
        if sum_truths == 0 {
            0.0
        } else {
            sum as f64 / sum_truths as f64
        }
    };
    println!(
        "{:>20} {:>10} {:>10} {:>9.4}",
        "full @ 15.0s",
        sum_full_15,
        sum_truths,
        recall(sum_full_15)
    );
    println!(
        "{:>20} {:>10} {:>10} {:>9.4}",
        "full @ 14.0s",
        sum_full_14,
        sum_truths,
        recall(sum_full_14)
    );
    println!(
        "{:>20} {:>10} {:>10} {:>9.4}",
        "scoped @ 14.0s",
        sum_scoped_14,
        sum_truths,
        recall(sum_scoped_14)
    );
    println!();

    // ----- Bootstrap CI deltas -----
    println!("{:-<78}", "");
    println!(
        "{:>32} {:>10} {:>10} {:>14}",
        "Δ vs full @ 15.0s", "Δ", "retention", "CI(95% Δ)"
    );
    println!("{:-<78}", "");

    let baseline = sum_full_15 as i64;

    let report = |label: &str, sum: u64, per_wav: &[(u32, u32)]| {
        let delta = sum as i64 - baseline;
        let retention = if baseline == 0 {
            0.0
        } else {
            sum as f64 / baseline as f64
        };
        let ci = bootstrap_recall_delta(&per_wav_full_15, per_wav, 1000, SEED);
        println!(
            "{:>32} {:>+10} {:>9.2}% {:>14}",
            label,
            delta,
            100.0 * retention,
            format!("[{:+.1},{:+.1}]", ci.ci_low, ci.ci_high),
        );
        (delta, retention, ci.ci_low, ci.ci_high)
    };

    report("full @ 14.0s", sum_full_14, &per_wav_full_14);
    let (_d_scoped, retention_scoped, ci_low_scoped, _ci_high_scoped) =
        report("scoped @ 14.0s", sum_scoped_14, &per_wav_scoped_14);
    println!();

    // ----- PROCEED gate -----
    println!("== PROCEED gate (scoped @ 14.0s vs full @ 15.0s) ==");
    println!("  PROCEED if retention >= 95% AND CI_low > -0.05 * baseline");
    let ci_floor = -0.05 * baseline as f64;
    println!(
        "  retention(scoped @ 14.0s) = {:.2}% (gate: >= 95%)",
        100.0 * retention_scoped
    );
    println!(
        "  CI_low = {:+.1}, floor = {:+.1} (gate: CI_low > floor)",
        ci_low_scoped, ci_floor
    );
    let proceed = retention_scoped >= 0.95 && ci_low_scoped > ci_floor;
    println!();
    println!(
        "AUTO-DECISION: {}",
        if proceed { "PROCEED" } else { "SHELVE" }
    );

    Ok(())
}
