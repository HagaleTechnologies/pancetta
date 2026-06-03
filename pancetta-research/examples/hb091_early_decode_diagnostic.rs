//! hb-091 — a8-style early-decode latency reduction (Session 1 diagnostic).
//!
//! ## Hypothesis (from bank)
//!
//! WSJT-X-Improved v3.x ships "a8" decoding technology that displays
//! messages from the in-QSO partner station ~0.5-1s earlier (release
//! notes for v3.0.0 250924, full support added in v3.0.0 251101). For
//! pancetta's autonomous coordinator, this would shave 0.5-1s off every
//! QSO leg's turnaround latency — translating to higher QSOs/hr under
//! fast-fade conditions.
//!
//! ## Session-1 question (this diagnostic)
//!
//! Before we wire any production scoping logic (scoped freq_bin pass at
//! t=13s), we need to know: how much sensitivity does the pancetta
//! decoder lose when fed only the first N seconds of the slot instead
//! of the full 15s? If a 2s early cutoff costs more than ~5% of decodes
//! relative to the full slot, the operational QSO/hr gain won't survive
//! the recall regression — SHELVE.
//!
//! This is a **truncation diagnostic on real-world hard-200 WAVs**, not
//! a synthetic-SNR experiment and not a production wiring step. It
//! measures the FULL search (no scoped freq_bin); the production a8 path
//! would be even cheaper (smaller search space), so the recall loss
//! measured here is an upper bound on what the production path would
//! lose at the equivalent cutoff.
//!
//! ## Method
//!
//! For each WAV in `research/corpus/curated/ft8/hard_200.manifest.json`:
//!   - load 12 kHz mono samples (180_000 for a full 15s window),
//!   - truncate to 5 lengths: 13.0s, 13.5s, 14.0s, 14.5s, 15.0s,
//!   - run `pancetta_ft8::Ft8Decoder::new(Ft8Config::default()).decode_window`
//!     on each truncation independently,
//!   - count how many jt9-truth messages (from `research/baselines/ft8/<sha>.json`)
//!     appear in pancetta's output (exact-string match on `.text.trim()`).
//!
//! ## Decision rule
//!
//! - **PROCEED to Session 2 (production wiring)** if recall at 14.0s ≥
//!   95% of recall at 15.0s, AND the 95% bootstrap CI on the delta
//!   `recall(14.0s) − recall(15.0s)` excludes a > 5% loss.
//! - **SHELVE** if the 14.0s recall drops below 95% retention or the CI
//!   indicates the loss could exceed 5% in production.
//!
//! Rationale: a8 buys at most 1-2s of latency. 5% recall loss = ~430 of
//! 8576 decodes lost on hard-200 — comfortably visible in QSO/hr math
//! and not worth a 5-15% QSO/hr lift.
//!
//! ## Primary source
//!
//! - WSJT-X-Improved Release_Notes.txt (DG2YCB):
//!   v3.0.0 (250924): "MTD 3-Stage now (partially) supports the new 'a8'
//!     decoding technology. This has the advantage that, under certain
//!     conditions, messages from the station you are in QSO with can be
//!     displayed 0.5 to 1 second earlier."
//!   v3.0.0 (251101): "The MTD now fully supports the 'a8' decoding
//!     technology."
//!   URL: <https://wsjt-x-improved.sourceforge.io/Release_Notes.txt>
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb091_early_decode_diagnostic
//!
//! Smoke run (first 5 WAVs):
//!   cargo run --release -p pancetta-research --example hb091_early_decode_diagnostic -- --max-wavs 5

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder, SAMPLE_RATE};
use pancetta_research::bootstrap_ci::bootstrap_recall_delta;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Truncation lengths in seconds. 15.0s is the baseline.
const TRUNCATIONS_S: &[f64] = &[13.0, 13.5, 14.0, 14.5, 15.0];

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

/// Load jt9 baseline message texts for a WAV by its sha256.
/// Returns empty vec if the baseline file is missing.
fn load_baseline_messages(ws: &Path, wav_sha256: &str) -> Result<Vec<String>> {
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
                .filter_map(|d| d.get("message").and_then(|m| m.as_str()))
                .map(|s| s.trim().to_string())
                .collect()
        })
        .unwrap_or_default())
}

/// Decode a truncated buffer and return the set of recovered jt9 truths.
fn recovered_from_truncation(samples_full: &[f32], cutoff_samples: usize, truths: &[String]) -> u32 {
    let cut = cutoff_samples.min(samples_full.len());
    let buf = &samples_full[..cut];
    let cfg = Ft8Config::default();
    let mut decoder = match Ft8Decoder::new(cfg) {
        Ok(d) => d,
        Err(_) => return 0,
    };
    let decoded = match decoder.decode_window(buf) {
        Ok(d) => d,
        Err(_) => return 0,
    };
    let our_texts: Vec<String> = decoded.iter().map(|m| m.text.trim().to_string()).collect();
    truths
        .iter()
        .filter(|t| our_texts.iter().any(|d| d == *t))
        .count() as u32
}

#[derive(Debug, Clone, Copy, Default)]
struct CellTotals {
    recovered_sum: u32,
    truth_sum: u32,
    wavs: u32,
}

fn main() -> Result<()> {
    // ----- Args -----
    let mut max_wavs: Option<usize> = None;
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
            other => anyhow::bail!("unknown arg: {other}"),
        }
    }

    let ws = workspace_root()?;
    let manifest_path = ws.join("research/corpus/curated/ft8/hard_200.manifest.json");

    // ----- Load manifest -----
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

    println!("hb-091 Session 1 — truncation diagnostic on hard-200");
    println!("  WAVs in tier:    {n_wavs}");
    println!("  Truncations (s): {:?}", TRUNCATIONS_S);
    println!("  Decoder:         pancetta-ft8 default Ft8Config (full search)");
    println!();

    // Per-truncation per-WAV records, aligned by WAV index for bootstrap CI.
    let mut per_wav_by_cutoff: BTreeMap<i64, Vec<(u32, u32)>> = BTreeMap::new();
    for &cutoff_s in TRUNCATIONS_S {
        per_wav_by_cutoff.insert((cutoff_s * 10.0).round() as i64, Vec::with_capacity(n_wavs));
    }

    // Per-truncation totals (for the recovery curve).
    let mut totals: BTreeMap<i64, CellTotals> = BTreeMap::new();
    for &cutoff_s in TRUNCATIONS_S {
        totals.insert((cutoff_s * 10.0).round() as i64, CellTotals::default());
    }

    let start = Instant::now();
    let mut last_progress = Instant::now();

    for (idx, (sha, wav_path)) in entries.iter().enumerate() {
        let samples = match load_wav_12k_mono(wav_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [{idx}] skip {wav_path:?}: {e}");
                // Push zero rows so per-WAV vectors stay aligned across cutoffs.
                for &c in TRUNCATIONS_S {
                    let key = (c * 10.0).round() as i64;
                    per_wav_by_cutoff.get_mut(&key).unwrap().push((0, 0));
                }
                continue;
            }
        };

        let truths = load_baseline_messages(&ws, sha)?;
        let truth_count = truths.len() as u32;

        for &cutoff_s in TRUNCATIONS_S {
            let cutoff_samples = (cutoff_s * SAMPLE_RATE as f64) as usize;
            let recovered = recovered_from_truncation(&samples, cutoff_samples, &truths);

            let key = (cutoff_s * 10.0).round() as i64;
            per_wav_by_cutoff
                .get_mut(&key)
                .unwrap()
                .push((recovered, truth_count));
            let cell = totals.get_mut(&key).unwrap();
            cell.recovered_sum += recovered;
            cell.truth_sum += truth_count;
            cell.wavs += 1;
        }

        // Progress every 30s.
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
                "  progress: {done}/{n_wavs} ({pct:.1}%) elapsed={elapsed_s:.0}s eta={eta_s:.0}s"
            );
            last_progress = Instant::now();
        }
    }

    let elapsed_total = start.elapsed().as_secs_f64();
    println!("\nDecode pass complete in {elapsed_total:.1}s\n");

    // ----- Report: recovery curve -----
    println!("{:-<60}", "");
    println!(
        "{:>8} {:>10} {:>10} {:>10}",
        "cutoff", "recovered", "truths", "recall"
    );
    println!("{:-<60}", "");

    let baseline_key = 150_i64; // 15.0s key
    let baseline_recovered = totals.get(&baseline_key).map(|c| c.recovered_sum).unwrap_or(0);

    for &cutoff_s in TRUNCATIONS_S {
        let key = (cutoff_s * 10.0).round() as i64;
        let c = totals.get(&key).copied().unwrap_or_default();
        let recall = if c.truth_sum == 0 {
            0.0
        } else {
            c.recovered_sum as f64 / c.truth_sum as f64
        };
        println!(
            "{:>8.1} {:>10} {:>10} {:>9.4}",
            cutoff_s, c.recovered_sum, c.truth_sum, recall
        );
    }
    println!();

    // ----- Report: retention vs 15.0s baseline -----
    println!("{:-<70}", "");
    println!(
        "{:>8} {:>10} {:>10} {:>10} {:>12} {:>14}",
        "cutoff", "rec(X)", "rec(15)", "Δ", "retention", "CI(95% Δ)"
    );
    println!("{:-<70}", "");

    let baseline_per_wav = per_wav_by_cutoff
        .get(&baseline_key)
        .cloned()
        .unwrap_or_default();

    for &cutoff_s in TRUNCATIONS_S {
        let key = (cutoff_s * 10.0).round() as i64;
        let per_wav = per_wav_by_cutoff.get(&key).cloned().unwrap_or_default();
        let c = totals.get(&key).copied().unwrap_or_default();

        let retention = if baseline_recovered == 0 {
            0.0
        } else {
            c.recovered_sum as f64 / baseline_recovered as f64
        };
        let delta = c.recovered_sum as i64 - baseline_recovered as i64;

        // Bootstrap CI on Δ = recovered(cutoff) − recovered(15.0s), per-WAV
        // aligned, 1000 resamples, seeded.
        let ci_str = if !per_wav.is_empty() && per_wav.len() == baseline_per_wav.len() {
            let ci = bootstrap_recall_delta(&baseline_per_wav, &per_wav, 1000, 091_2026_06_02);
            format!("[{:+.1},{:+.1}]", ci.ci_low, ci.ci_high)
        } else {
            "n/a".to_string()
        };

        println!(
            "{:>8.1} {:>10} {:>10} {:>+10} {:>11.2}% {:>14}",
            cutoff_s,
            c.recovered_sum,
            baseline_recovered,
            delta,
            100.0 * retention,
            ci_str
        );
    }
    println!();

    // ----- Decision -----
    let key_14 = 140_i64;
    let rec_14 = totals.get(&key_14).map(|c| c.recovered_sum).unwrap_or(0);
    let retention_14 = if baseline_recovered == 0 {
        0.0
    } else {
        rec_14 as f64 / baseline_recovered as f64
    };
    let per_wav_14 = per_wav_by_cutoff.get(&key_14).cloned().unwrap_or_default();
    let ci_14 = if !per_wav_14.is_empty() && per_wav_14.len() == baseline_per_wav.len() {
        Some(bootstrap_recall_delta(
            &baseline_per_wav,
            &per_wav_14,
            1000,
            091_2026_06_02,
        ))
    } else {
        None
    };

    println!("== Decision rule (a8 Session-1 PROCEED gate) ==");
    println!("  PROCEED if retention(14.0s) >= 95% AND bootstrap CI excludes >5% loss");
    println!("  i.e. CI_low > -0.05 * baseline_recovered = {:.1}", -0.05 * baseline_recovered as f64);
    println!();

    let ci_floor = -0.05 * baseline_recovered as f64;
    let mut proceed = retention_14 >= 0.95;
    if let Some(ci) = &ci_14 {
        if ci.ci_low < ci_floor {
            proceed = false;
        }
        println!(
            "  retention(14.0s) = {:.2}% | Δ = {:+} | CI = [{:+.1}, {:+.1}]",
            100.0 * retention_14,
            rec_14 as i64 - baseline_recovered as i64,
            ci.ci_low,
            ci.ci_high
        );
    } else {
        println!("  retention(14.0s) = {:.2}% (no CI)", 100.0 * retention_14);
    }
    println!();
    println!(
        "AUTO-DECISION: {}",
        if proceed { "PROCEED" } else { "SHELVE" }
    );

    Ok(())
}
