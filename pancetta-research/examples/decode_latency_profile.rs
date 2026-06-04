//! Decoder wall-clock latency profile (generic + hb-091 mechanism check).
//!
//! Measures `Ft8Decoder` wall-clock per WAV on `hard_200` (or a configurable
//! subset). Reports p50/p90/p95/p99/max + a 10-bucket histogram for each
//! arm. Includes hardware context (CPU model, logical cores, OS) so results
//! are reproducible across machines.
//!
//! ## Two use cases
//!
//! 1. **hb-091 mechanism check** (`--scoped --half-width 5`): compares full
//!    decode vs scoped decode at the in-QSO partner's freq_bin range. The
//!    load-bearing question: does scoped reliably save wall-clock at the
//!    p95/p99 tails (where late-firing risk lives)?
//!
//!    Pancetta's DSP already fires decode at t=13s into the slot (with a
//!    15s buffer covering slot N-1's last 2s + slot N's first 13s, full
//!    FT8 message contained). The slack between decode-completion and
//!    next-slot TX boundary (t=15) is ~1.7s. Scoped's wall-clock win only
//!    matters operationally if the full-decode tail approaches this slack.
//!
//! 2. **Hardware-tier baseline** (default, no `--scoped`): full-decode
//!    wall-clock distribution on a known reference machine. Future
//!    hardware-tier work (adaptive multipass / OSD depth on lower-tier
//!    MiniPCs) compares against this baseline to set tier-appropriate
//!    decode budgets.
//!
//! ## Method
//!
//! For each WAV in `hard_200.manifest.json`:
//!   1. Load full 15s samples (12 kHz mono).
//!   2. WARMUP: decode once without timing (stabilizes CPU caches +
//!      heap allocations).
//!   3. MEASURE full: `Instant::now() ; decode_window(buf) ; elapsed`.
//!   4. If `--scoped`: pick the FIRST jt9 truth in the baseline, derive
//!      freq_bin = (truth.freq_hz / 6.25).round() as usize, MEASURE
//!      scoped: `decode_window_scoped(buf, (bin - HW)..=(bin + HW))`.
//!      Skips the WAV (scoped arm only) if no jt9 baseline exists.
//!
//! After all WAVs:
//!   - Sort each arm's timings; report p50/p90/p95/p99/max.
//!   - 10-bucket histogram across the [min, max] range of the full arm.
//!   - Δ at each percentile (scoped vs full).
//!
//! ## Notes
//!
//! - The diagnostic creates a fresh `Ft8Decoder` per arm per WAV — so
//!   per-call wall-clock includes constructor cost. For production
//!   simulation in pancetta's coordinator the decoder is reused across
//!   slots, so production wall-clock will be slightly lower than these
//!   numbers (~few ms saved per call on the constructor).
//! - Hardware reporting uses `sysctl` on macOS, `/proc/cpuinfo` on Linux.
//!
//! Run (full hard-200, both arms):
//!   cargo run --release -p pancetta-research --example decode_latency_profile -- --scoped --half-width 5
//!
//! Smoke (5 WAVs, full only):
//!   cargo run --release -p pancetta-research --example decode_latency_profile -- --max-wavs 5

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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

/// First jt9 truth's freq_hz, or None if the baseline file is missing or empty.
fn first_truth_freq_hz(ws: &Path, wav_sha256: &str) -> Result<Option<f64>> {
    let p = ws
        .join("research/baselines/ft8")
        .join(format!("{wav_sha256}.json"));
    if !p.exists() {
        return Ok(None);
    }
    let s = std::fs::read_to_string(&p)?;
    let v: Value = serde_json::from_str(&s)?;
    Ok(v.get("decodes")
        .and_then(|d| d.as_array())
        .and_then(|arr| arr.first())
        .and_then(|d| d.get("freq_hz"))
        .and_then(|f| f.as_f64()))
}

/// Time a full decode of the buffer. Returns the wall-clock elapsed.
fn time_full_decode(samples: &[f32]) -> Result<Duration> {
    let mut decoder = Ft8Decoder::new(Ft8Config::default())?;
    let start = Instant::now();
    let _ = decoder.decode_window(samples)?;
    Ok(start.elapsed())
}

/// Time a scoped decode at `center_bin ± half_width`. Returns the wall-clock.
fn time_scoped_decode(samples: &[f32], center_bin: usize, half_width: usize) -> Result<Duration> {
    let mut decoder = Ft8Decoder::new(Ft8Config::default())?;
    let lo = center_bin.saturating_sub(half_width);
    let hi = center_bin.saturating_add(half_width);
    let start = Instant::now();
    let _ = decoder.decode_window_scoped(samples, lo..=hi)?;
    Ok(start.elapsed())
}

/// Warmup a decoder (call once, ignore output) to stabilize CPU caches.
fn warmup(samples: &[f32]) {
    if let Ok(mut decoder) = Ft8Decoder::new(Ft8Config::default()) {
        let _ = decoder.decode_window(samples);
    }
}

fn cpu_model() -> String {
    if cfg!(target_os = "macos") {
        Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    } else if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|s| {
                s.lines().find(|l| l.starts_with("model name")).map(|l| {
                    l.trim_start_matches("model name")
                        .trim_start_matches(": ")
                        .trim()
                        .to_string()
                })
            })
            .unwrap_or_else(|| "unknown".to_string())
    } else {
        "unknown".to_string()
    }
}

fn cpu_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0)
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = (p * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn print_stats(label: &str, samples: &[Duration]) {
    if samples.is_empty() {
        println!("  {:<14} (no samples)", label);
        return;
    }
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort();
    let p50 = percentile(&sorted, 0.50);
    let p90 = percentile(&sorted, 0.90);
    let p95 = percentile(&sorted, 0.95);
    let p99 = percentile(&sorted, 0.99);
    let max = *sorted.last().unwrap();
    let mean: f64 = sorted.iter().map(|d| ms(*d)).sum::<f64>() / sorted.len() as f64;
    println!(
        "  {:<14} n={} mean={:.1}ms p50={:.1} p90={:.1} p95={:.1} p99={:.1} max={:.1}",
        label,
        samples.len(),
        mean,
        ms(p50),
        ms(p90),
        ms(p95),
        ms(p99),
        ms(max),
    );
}

fn print_histogram(label: &str, samples: &[Duration], reference: &[Duration]) {
    if samples.is_empty() || reference.is_empty() {
        return;
    }
    // Use the reference arm's range so both histograms share x-axis.
    let mut ref_sorted: Vec<Duration> = reference.to_vec();
    ref_sorted.sort();
    let lo = ms(ref_sorted[0]);
    let hi = ms(*ref_sorted.last().unwrap());
    let nbuckets = 10;
    let width = ((hi - lo) / nbuckets as f64).max(1.0);
    let mut buckets = vec![0usize; nbuckets];
    for s in samples {
        let v = ms(*s);
        let idx = ((v - lo) / width) as usize;
        buckets[idx.min(nbuckets - 1)] += 1;
    }
    let max_count = *buckets.iter().max().unwrap_or(&1);
    println!(
        "  {:<14} histogram (x = wall-clock ms, range {:.0}..{:.0}):",
        label, lo, hi
    );
    for (i, &c) in buckets.iter().enumerate() {
        let bar_w = if max_count > 0 {
            ((c * 40) / max_count).max(if c > 0 { 1 } else { 0 })
        } else {
            0
        };
        let lo_b = lo + i as f64 * width;
        let hi_b = lo + (i + 1) as f64 * width;
        let bar: String = std::iter::repeat('█').take(bar_w).collect();
        println!("    [{:>6.1}..{:<6.1}] {:>4}  {}", lo_b, hi_b, c, bar);
    }
}

fn main() -> Result<()> {
    // ----- Args -----
    let mut max_wavs: Option<usize> = None;
    let mut include_scoped = false;
    let mut half_width: usize = 5;
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
            "--scoped" => include_scoped = true,
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

    println!("== Decoder Latency Profile ==");
    println!("  CPU model:     {}", cpu_model());
    println!("  Logical cores: {}", cpu_cores());
    println!("  Target arch:   {}", std::env::consts::ARCH);
    println!("  OS:            {}", std::env::consts::OS);
    println!("  Build profile: release");
    println!("  WAVs in tier:  {n_wavs}");
    if include_scoped {
        println!(
            "  Scoped arm:    ON, half-width ±{} bins (~±{:.1} Hz)",
            half_width,
            half_width as f64 * 6.25
        );
    } else {
        println!("  Scoped arm:    OFF (full decode only)");
    }
    println!();

    let mut full_durs: Vec<Duration> = Vec::with_capacity(n_wavs);
    let mut scoped_durs: Vec<Duration> = Vec::with_capacity(n_wavs);

    let start = Instant::now();
    let mut last_progress = Instant::now();
    let mut skipped_scoped = 0usize;

    for (idx, (sha, wav_path)) in entries.iter().enumerate() {
        let samples = match load_wav_12k_mono(wav_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [{idx}] skip {wav_path:?}: {e}");
                continue;
            }
        };

        // Truncate / pad to exactly 180_000 samples (15s) so all WAVs hit the
        // decoder at the same buffer length — wall-clock varies with buffer
        // length and we want apples-to-apples per-WAV.
        let target_len = 180_000;
        let buf: Vec<f32> = if samples.len() >= target_len {
            samples[..target_len].to_vec()
        } else {
            let mut v = samples.clone();
            v.resize(target_len, 0.0);
            v
        };

        // Warmup pass — discard.
        warmup(&buf);

        // Full decode.
        match time_full_decode(&buf) {
            Ok(d) => full_durs.push(d),
            Err(e) => eprintln!("  [{idx}] full decode err: {e}"),
        }

        // Scoped decode (optional).
        if include_scoped {
            match first_truth_freq_hz(&ws, sha)? {
                Some(freq_hz) => {
                    let bin = (freq_hz / 6.25).round() as usize;
                    match time_scoped_decode(&buf, bin, half_width) {
                        Ok(d) => scoped_durs.push(d),
                        Err(e) => eprintln!("  [{idx}] scoped decode err: {e}"),
                    }
                }
                None => skipped_scoped += 1,
            }
        }

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

    let elapsed = start.elapsed().as_secs_f64();
    println!("\nMeasurement complete in {elapsed:.1}s");
    if include_scoped && skipped_scoped > 0 {
        println!(
            "  (scoped arm skipped {} WAVs lacking a jt9 baseline)",
            skipped_scoped
        );
    }
    println!();

    // ----- Summary stats -----
    println!("{:-<78}", "");
    println!("Summary (wall-clock per decode):");
    print_stats("full", &full_durs);
    if include_scoped {
        print_stats("scoped", &scoped_durs);
    }
    println!();

    if include_scoped && !scoped_durs.is_empty() && !full_durs.is_empty() {
        let mut full_sorted: Vec<Duration> = full_durs.clone();
        full_sorted.sort();
        let mut scoped_sorted: Vec<Duration> = scoped_durs.clone();
        scoped_sorted.sort();
        println!("Δ (scoped vs full) at each percentile:");
        for &p in &[0.50, 0.90, 0.95, 0.99] {
            let f = percentile(&full_sorted, p);
            let s = percentile(&scoped_sorted, p);
            let delta_ms = ms(s) - ms(f);
            let ratio = if ms(f) > 0.0 { ms(s) / ms(f) } else { 0.0 };
            println!(
                "  p{:<2}  full={:>7.1}ms  scoped={:>7.1}ms  Δ={:>+7.1}ms ({:.2}x)",
                (p * 100.0) as i64,
                ms(f),
                ms(s),
                delta_ms,
                ratio,
            );
        }
        println!();
    }

    // ----- Histograms -----
    print_histogram("full", &full_durs, &full_durs);
    if include_scoped {
        println!();
        print_histogram("scoped", &scoped_durs, &full_durs);
    }

    Ok(())
}
