//! Batch 88 — hb-249 dt audit: where does pancetta's reported
//! `time_offset` sit relative to the sample-accurate signal position?
//!
//! Three independent measurements:
//!
//! **Part A (synthetic ground truth)**: modulate a known message, embed
//! its waveform at an exactly known start sample `x0` in a noisy 15 s
//! buffer, decode with the default config, and report
//! `reported_dt*12000 - x0`. True position is exact by construction —
//! this maps the bias as a function of sub-step position, frequency and
//! base dt, and exposes the quantization grain.
//!
//! **Part B (real corpus, LS fit)**: for the strongest decodes of the 20
//! Batch 86 kill-switch slots, re-synthesize each decode's waveform from
//! its CRC-verified tone_symbols (modulator at 1500 Hz anchor + Hilbert
//! heterodyne, exactly as batch86) and grid-search (delta-f, delta-t)
//! for the minimum-residual per-block LS fit. `best_dt` = fitted
//! position minus reported position, in samples.
//!
//! **Part C (cross-decoder scale)**: for the same slots, match pancetta
//! decodes to ft8_lib truth by `hash_normalize_message` (unique matches
//! only) and report `pancetta_dt - ft8lib_time_sec`. NOTE ft8_lib's own
//! convention may carry the same fixed offset (its monitor.c sliding
//! frame is what pancetta's spectrogram replicates) — Part A/B are the
//! ground-truth arbiters; this part only measures relative scale.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch88_dt_audit
//!
//! Optional: pass `--fast` to halve Part B's decode list (top-1/slot).

use anyhow::{Context, Result};
use num_complex::Complex;
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, NUM_SYMBOLS};
use pancetta_research::metrics::hash_normalize_message;
use rand::SeedableRng;
use rand_distr::{Distribution, Normal};
use rustfft::FftPlanner;
use serde::Deserialize;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: f64 = 12_000.0;
const MOD_ANCHOR_HZ: f64 = 1500.0;
const MIN_OVERLAP_SAMPLES: i64 = 8 * 1920;

#[derive(Deserialize)]
struct WorkList {
    entries: Vec<WorkEntry>,
}

#[derive(Deserialize)]
struct WorkEntry {
    wav_path: String,
    wav_sha256: String,
}

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

/// FFT-based analytic signal (Hilbert), copied from batch86.
fn analytic(s: &[f32]) -> Vec<Complex<f64>> {
    let n = s.len();
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(n);
    let ifft = planner.plan_fft_inverse(n);
    let mut buf: Vec<Complex<f64>> = s.iter().map(|&x| Complex::new(x as f64, 0.0)).collect();
    fft.process(&mut buf);
    let half = n.div_ceil(2);
    for (k, v) in buf.iter_mut().enumerate() {
        if k == 0 || (n % 2 == 0 && k == n / 2) {
        } else if k < half {
            *v *= 2.0;
        } else {
            *v = Complex::new(0.0, 0.0);
        }
    }
    ifft.process(&mut buf);
    let scale = 1.0 / n as f64;
    for v in buf.iter_mut() {
        *v *= scale;
    }
    buf
}

fn synth_analytic_anchor(symbols: &[u8; NUM_SYMBOLS]) -> Result<Vec<Complex<f64>>> {
    let mut modulator = Ft8Modulator::new(12_000, MOD_ANCHOR_HZ, 1.0)
        .map_err(|e| anyhow::anyhow!("Ft8Modulator::new: {e}"))?;
    let wave = modulator
        .modulate_symbols(symbols, 0.0)
        .map_err(|e| anyhow::anyhow!("modulate_symbols: {e}"))?;
    Ok(analytic(&wave))
}

fn shift_to(z: &[Complex<f64>], freq_hz: f64) -> (Vec<f64>, Vec<f64>) {
    let w = 2.0 * std::f64::consts::PI * (freq_hz - MOD_ANCHOR_HZ) / SAMPLE_RATE;
    let mut s = Vec::with_capacity(z.len());
    let mut sq = Vec::with_capacity(z.len());
    for (i, zi) in z.iter().enumerate() {
        let v = zi * Complex::from_polar(1.0, w * i as f64);
        s.push(v.re);
        sq.push(v.im);
    }
    (s, sq)
}

struct Fit {
    res_ratio: f64,
}

/// Per-block complex-amplitude LS fit, copied from batch86.
fn per_block_fit(x: &[f32], s: &[f64], sq: &[f64], offset: i64, n_blocks: usize) -> Option<Fit> {
    let n = x.len() as i64;
    let m = s.len() as i64;
    if (offset + m).min(n) - offset.max(0) < MIN_OVERLAP_SAMPLES {
        return None;
    }
    let block_len = s.len().div_ceil(n_blocks);
    let mut n_solves = 0usize;
    let mut res_energy = 0f64;
    let mut total_xx = 0f64;
    for blk in 0..n_blocks {
        let j0 = blk * block_len;
        let j1 = ((blk + 1) * block_len).min(s.len());
        let i0 = (offset + j0 as i64).max(0);
        let i1 = (offset + j1 as i64).min(n);
        if i1 <= i0 {
            continue;
        }
        let (mut ss, mut sxq, mut qq, mut xs, mut xq, mut xx) =
            (0f64, 0f64, 0f64, 0f64, 0f64, 0f64);
        for i in i0..i1 {
            let xi = x[i as usize] as f64;
            let j = (i - offset) as usize;
            let (si, qi) = (s[j], sq[j]);
            ss += si * si;
            sxq += si * qi;
            qq += qi * qi;
            xs += xi * si;
            xq += xi * qi;
            xx += xi * xi;
        }
        total_xx += xx;
        let det = ss * qq - sxq * sxq;
        if det.abs() <= 1e-9 * ss.max(1e-30) * qq.max(1e-30) {
            res_energy += xx;
            continue;
        }
        let a = (xs * qq - xq * sxq) / det;
        let b = (xq * ss - xs * sxq) / det;
        res_energy += (xx - a * xs - b * xq).max(0.0);
        n_solves += 1;
    }
    if n_solves == 0 {
        return None;
    }
    Some(Fit {
        res_ratio: res_energy / total_xx.max(1e-30),
    })
}

/// Two-stage (delta-f, delta-t) search for the minimum-residual fit
/// around (freq0, offset0). Returns (best_df, best_dt, best_res, nominal_res).
fn fit_search(
    x: &[f32],
    z: &[Complex<f64>],
    freq0: f64,
    offset0: i64,
) -> Option<(f64, i64, f64, f64)> {
    let mut best: Option<(f64, i64, f64)> = None;
    let mut nominal = f64::NAN;
    // Coarse: df +/-1.6 Hz (work coords quantized to the 3.125 Hz
    // half-tone grid) x dt +/-4800 samples in 120-sample steps.
    for k in -8..=8i32 {
        let df = k as f64 * 0.2;
        let (s, sq) = shift_to(z, freq0 + df);
        let mut dt = -4800i64;
        while dt <= 4800 {
            if let Some(fit) = per_block_fit(x, &s, &sq, offset0 + dt, 10) {
                if k == 0 && dt == 0 {
                    nominal = fit.res_ratio;
                }
                if best.is_none() || fit.res_ratio < best.unwrap().2 {
                    best = Some((df, dt, fit.res_ratio));
                }
            }
            dt += 120;
        }
    }
    let (df0, dt0, _) = best?;
    // Refine: df +/-0.25 Hz in 0.05 steps x dt +/-120 in 20-sample steps.
    for k in -5..=5i32 {
        let df = df0 + k as f64 * 0.05;
        let (s, sq) = shift_to(z, freq0 + df);
        for j in -6..=6i64 {
            let dt = dt0 + j * 20;
            if let Some(fit) = per_block_fit(x, &s, &sq, offset0 + dt, 10) {
                if fit.res_ratio < best.unwrap().2 {
                    best = Some((df, dt, fit.res_ratio));
                }
            }
        }
    }
    let (df, dt, res) = best?;
    Some((df, dt, res, nominal))
}

fn summarize(label: &str, deltas: &[f64]) {
    if deltas.is_empty() {
        println!("{label}: no samples");
        return;
    }
    let mut v: Vec<f64> = deltas.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    let mean = v.iter().sum::<f64>() / n as f64;
    let median = v[n / 2];
    let var = v.iter().map(|d| (d - mean) * (d - mean)).sum::<f64>() / n as f64;
    println!(
        "{label}: n={n} mean={mean:+.1} median={median:+.1} sd={:.1} min={:+.1} max={:+.1} (samples)",
        var.sqrt(),
        v[0],
        v[n - 1]
    );
    // Histogram in 240-sample (20 ms) buckets over [-2880, +2880).
    let mut hist = [0usize; 24];
    for d in &v {
        let b = ((d + 2880.0) / 240.0).floor();
        if (0.0..24.0).contains(&b) {
            hist[b as usize] += 1;
        }
    }
    print!("  hist[-2880..2880, 240/bucket]: ");
    for (i, c) in hist.iter().enumerate() {
        if *c > 0 {
            print!("{:+}:{c} ", -2880 + (i as i64) * 240);
        }
    }
    println!();
}

fn pearson(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len() as f64;
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    let mut syy = 0.0;
    for (x, y) in xs.iter().zip(ys) {
        sxy += (x - mx) * (y - my);
        sxx += (x - mx) * (x - mx);
        syy += (y - my) * (y - my);
    }
    sxy / (sxx.sqrt() * syy.sqrt()).max(1e-30)
}

// ============================================================
// Part A: synthetic ground truth
// ============================================================

fn part_a() -> Result<()> {
    println!("== Part A: synthetic ground truth (reported - true, samples) ==");
    let symbols = Ft8Encoder::new()
        .encode_message("CQ W9XYZ EN50", None)
        .map_err(|e| anyhow::anyhow!("encode: {e}"))?;
    let mut rng = rand::rngs::StdRng::seed_from_u64(0xB88);
    let noise = Normal::new(0.0f64, 0.02).unwrap();
    let cfg = Ft8Config::default();
    let mut deltas: Vec<f64> = Vec::new();
    let mut rows: Vec<String> = Vec::new();
    for &freq in &[887.5f64, 1512.5, 2287.5, 1505.6] {
        let mut modulator = Ft8Modulator::new(12_000, freq, 0.5)
            .map_err(|e| anyhow::anyhow!("Ft8Modulator::new: {e}"))?;
        let wave = modulator
            .modulate_symbols(&symbols, 0.0)
            .map_err(|e| anyhow::anyhow!("modulate_symbols: {e}"))?;
        for &base in &[6000usize, 28080] {
            for k in 0..8usize {
                let x0 = base + k * 240; // sweep one symbol period in 1/8 steps
                let mut buf: Vec<f32> = (0..180_000)
                    .map(|_| noise.sample(&mut rng) as f32)
                    .collect();
                for (i, &w) in wave.iter().enumerate() {
                    if x0 + i < buf.len() {
                        buf[x0 + i] += w;
                    }
                }
                let mut decoder = Ft8Decoder::new(cfg.clone())
                    .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
                let decoded = decoder
                    .decode_window(&buf)
                    .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
                let hit = decoded.iter().find(|d| d.text == "CQ W9XYZ EN50");
                match hit {
                    Some(d) => {
                        let rep = d.time_offset * SAMPLE_RATE;
                        let delta = rep - x0 as f64;
                        deltas.push(delta);
                        rows.push(format!(
                            "  f={freq:7.1} x0={x0:6} rep={rep:8.0} delta={delta:+6.0}"
                        ));
                    }
                    None => rows.push(format!("  f={freq:7.1} x0={x0:6} NOT DECODED")),
                }
            }
        }
    }
    for r in &rows {
        println!("{r}");
    }
    summarize("Part A delta", &deltas);
    Ok(())
}

// ============================================================
// Part B: real-corpus LS-fit audit
// ============================================================

fn part_b(worklist: &WorkList, fast: bool) -> Result<()> {
    println!("\n== Part B: real-corpus LS-fit audit (fitted - reported, samples) ==");
    let cfg = Ft8Config::default();
    let per_slot = if fast { 1 } else { 2 };
    let mut deltas: Vec<f64> = Vec::new();
    let mut freqs: Vec<f64> = Vec::new();
    let mut dts: Vec<f64> = Vec::new();
    for entry in &worklist.entries {
        let samples = load_wav(Path::new(&entry.wav_path))?;
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let mut greedy = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        greedy.sort_by(|a, b| b.snr_db.partial_cmp(&a.snr_db).unwrap());
        let label = Path::new(&entry.wav_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let mut taken = 0;
        for d in &greedy {
            if taken >= per_slot {
                break;
            }
            let Some(t) = d.tone_symbols.as_ref().filter(|t| t.len() >= NUM_SYMBOLS) else {
                continue;
            };
            let mut symbols = [0u8; NUM_SYMBOLS];
            symbols.copy_from_slice(&t[..NUM_SYMBOLS]);
            let z = synth_analytic_anchor(&symbols)?;
            let offset0 = (d.time_offset * SAMPLE_RATE).round() as i64;
            let Some((df, dt, res, nominal)) =
                fit_search(&samples, &z, d.frequency_offset, offset0)
            else {
                continue;
            };
            taken += 1;
            // Only count clean locks toward the distribution: the fit
            // must explain a visible energy fraction at the best cell.
            let locked = res < 0.985 && res < nominal - 0.005;
            println!(
                "  {label} '{}' snr={:+3.0} f={:7.1} rep_dt={:5.2}s -> best df={df:+.2} dt={dt:+5} res={res:.4} (nom {nominal:.4}){}",
                d.text,
                d.snr_db,
                d.frequency_offset,
                d.time_offset,
                if locked { "" } else { "  [no lock, excluded]" }
            );
            if locked {
                deltas.push(dt as f64);
                freqs.push(d.frequency_offset);
                dts.push(d.time_offset);
            }
        }
    }
    summarize("Part B delta (fitted - reported)", &deltas);
    if deltas.len() >= 3 {
        println!(
            "  corr(delta, freq) = {:+.3}; corr(delta, reported_dt) = {:+.3}",
            pearson(&deltas, &freqs),
            pearson(&deltas, &dts)
        );
    }
    Ok(())
}

// ============================================================
// Part C: ft8_lib truth dt comparison (hash-normalized matching)
// ============================================================

fn part_c(ws: &Path, worklist: &WorkList) -> Result<()> {
    println!("\n== Part C: pancetta dt - ft8_lib time_sec (matched decodes, samples) ==");
    #[derive(Deserialize)]
    struct TruthFile {
        decodes: Vec<TruthDecode>,
    }
    #[derive(Deserialize)]
    struct TruthDecode {
        message: String,
        time_sec: f64,
    }
    let cfg = Ft8Config::default();
    let mut deltas: Vec<f64> = Vec::new();
    for entry in &worklist.entries {
        let truth_path = ws
            .join("research/baselines/ft8")
            .join(format!("{}.ft8lib.json", entry.wav_sha256));
        let Ok(txt) = std::fs::read_to_string(&truth_path) else {
            continue;
        };
        let truth: TruthFile = serde_json::from_str(&txt)?;
        let samples = load_wav(Path::new(&entry.wav_path))?;
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        // Unique normalized texts on both sides to avoid mismatched pairing.
        let mut t_map: std::collections::HashMap<String, Vec<f64>> = Default::default();
        for t in &truth.decodes {
            t_map
                .entry(hash_normalize_message(&t.message))
                .or_default()
                .push(t.time_sec);
        }
        let mut p_map: std::collections::HashMap<String, Vec<f64>> = Default::default();
        for d in &decoded {
            p_map
                .entry(hash_normalize_message(&d.text))
                .or_default()
                .push(d.time_offset);
        }
        for (key, pv) in &p_map {
            if pv.len() != 1 {
                continue;
            }
            if let Some(tv) = t_map.get(key) {
                if tv.len() == 1 {
                    deltas.push((pv[0] - tv[0]) * SAMPLE_RATE);
                }
            }
        }
    }
    summarize("Part C delta (pancetta - ft8_lib)", &deltas);
    Ok(())
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let fast = std::env::args().any(|a| a == "--fast");
    let worklist: WorkList = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hb104_kill_switch.json"),
    )?)?;
    part_a()?;
    part_b(&worklist, fast)?;
    part_c(&ws, &worklist)?;
    Ok(())
}
