//! Batch 98 — hb-252 BICM-ID FP-control graduation run.
//!
//! Batch 97 confirmed the SOMAP rescue mechanism (+0.384 dB synthetic
//! threshold shift at `bicm_id_iterations = 2`) but the hard_200/50
//! spot showed ΔTP +3 / ΔFP +21 — rescued BP runs on noise candidates
//! sometimes converge to wrong CRC-passing codewords. Batch 98 adds
//! three FP controls (near-converged gate on the seed-BP
//! unsatisfied-check count, `decode_origin = Some(7)` stamping, and
//! unconditional suspicion scrutiny for rescued decodes) and measures
//! the gated config.
//!
//! Parts (select with `BATCH98_PART`):
//!   instrument  hard_200 first N WAVs, iters=2, gate disabled
//!               (max_unsat=83), instrumentation on. Prints the
//!               unsatisfied-check distribution of (a) rescues that
//!               produced a truth-matching decode, (b) rescues that
//!               produced a wrong-CRC FP decode, (c) failed rescues,
//!               plus cumulative keep-fractions per threshold.
//!   spot        hard_200 first N WAVs, iters {0, 2} at the gated
//!               config — the Batch 97 Part B re-run.
//!   synth       quick synthetic SNR sweep (default 10 trials/point),
//!               iters {0, 2} at the gated config; threshold shift.
//!   full        full-corpus run, iters {0, 2} at the gated config.
//!               `BATCH98_CORPUS` = raw_530_full (default) | hard_1000.
//!
//! Env:
//!   BATCH98_PART=instrument|spot|synth|full   (required)
//!   BATCH98_MAX_UNSAT=26     near-converged gate (gated parts)
//!   BATCH98_WAVS=50          WAV count (instrument/spot)
//!   BATCH98_TRIALS=10        trials/point (synth)
//!   BATCH98_CORPUS=raw_530_full.manifest.json | hard_1000.manifest.json
//!
//! Pre-registered verdict bars (see
//! `research/notes/2026-06-12-batch98-bicm-id-gated.md`):
//!   spot proceed:  ΔFP ≤ 2×ΔTP with ΔTP > 0
//!   SHIP default 2: raw_530_full ΔTP ≥ +20 AND ΔFP ≤ +50 AND
//!                   hard_1000 consistent AND wall ≤ +25%
//!   synth sanity:  gated shift stays ≥ 0.2 dB
//!
//! Run:
//!   BATCH98_PART=spot cargo run --release -p pancetta-research \
//!     --example batch98_bicm_id_gated

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES};
use pancetta_research::metrics::hash_normalize_message;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, Seek};
use std::path::{Path, PathBuf};

const SAMPLE_RATE: f32 = 12_000.0;

/// 20 distinct standard messages (CQ / grid / report / RR73 shapes) —
/// same plant as batch97.
const MESSAGES: [&str; 20] = [
    "CQ K1ABC FN42",
    "CQ W9XYZ EN50",
    "CQ N5DDD DM79",
    "CQ G4AAA IO91",
    "CQ JA1BBB PM95",
    "K1ABC W9XYZ EN50",
    "W9XYZ K1ABC -07",
    "K1ABC W9XYZ R-09",
    "W9XYZ K1ABC RR73",
    "K1ABC W9XYZ 73",
    "N5DDD G4AAA IO91",
    "G4AAA N5DDD -15",
    "N5DDD G4AAA R-18",
    "G4AAA N5DDD RR73",
    "CQ VK3CCC QF22",
    "VK3CCC JA1BBB PM95",
    "JA1BBB VK3CCC +03",
    "VK3CCC JA1BBB R+01",
    "JA1BBB VK3CCC RRR",
    "CQ PY2DDD GG66",
];

fn gaussian_noise(rng: &mut StdRng, n: usize, sigma: f32) -> Vec<f32> {
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let u1: f32 = rng.gen_range(f32::EPSILON..1.0);
        let u2: f32 = rng.gen_range(0.0..1.0);
        let mag = (-2.0 * u1.ln()).sqrt();
        let z0 = mag * (2.0 * std::f32::consts::PI * u2).cos();
        let z1 = mag * (2.0 * std::f32::consts::PI * u2).sin();
        out.push(z0 * sigma);
        i += 1;
        if i < n {
            out.push(z1 * sigma);
            i += 1;
        }
    }
    out
}

fn signal_power(samples: &[f32]) -> f32 {
    samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32
}

/// σ for AWGN such that SNR in 2500 Hz reference bandwidth = snr_db.
fn sigma_for_snr_db(p_signal: f32, snr_db: f32) -> f32 {
    let bw: f32 = 2500.0;
    let p_n_in_bw = p_signal / 10.0f32.powf(snr_db / 10.0);
    let nyquist = SAMPLE_RATE / 2.0;
    (p_n_in_bw * nyquist / bw).sqrt()
}

fn cfg(iters: usize, max_unsat: usize) -> Ft8Config {
    Ft8Config {
        bicm_id_iterations: iters,
        bicm_id_max_unsatisfied_checks: max_unsat,
        ..Ft8Config::default()
    }
}

fn threshold_50(snrs: &[f32], rates: &[f64]) -> Option<f64> {
    if rates[0] >= 0.5 {
        return Some(snrs[0] as f64);
    }
    for k in 1..rates.len() {
        if rates[k] >= 0.5 && rates[k - 1] < 0.5 {
            let span = (snrs[k] - snrs[k - 1]) as f64;
            let frac = (0.5 - rates[k - 1]) / (rates[k] - rates[k - 1]);
            return Some(snrs[k - 1] as f64 + frac * span);
        }
    }
    None
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

fn load_ft8lib_truth(ws: &Path, sha: &str) -> Option<HashSet<String>> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{sha}.ft8lib.json"));
    let txt = std::fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    Some(
        v["decodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["message"].as_str())
            .map(hash_normalize_message)
            .collect(),
    )
}

fn manifest_entries(ws: &Path, manifest_name: &str) -> Result<Vec<(String, String)>> {
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8").join(manifest_name),
    )?)?;
    Ok(manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .filter_map(|e| {
            Some((
                e["wav_path"].as_str()?.to_string(),
                e["wav_sha256"].as_str()?.to_string(),
            ))
        })
        .collect())
}

/// Decode a list of (wav, truth) entries at one config; returns
/// (tp, fp, wall_s, n_scored).
fn run_corpus(
    ws: &Path,
    entries: &[(String, String)],
    config: &Ft8Config,
) -> Result<(usize, usize, f64, usize)> {
    let mut tps = 0usize;
    let mut fps = 0usize;
    let mut scored = 0usize;
    let t0 = std::time::Instant::now();
    for (wav_path, sha) in entries {
        let Some(truth) = load_ft8lib_truth(ws, sha) else {
            continue;
        };
        let samples = load_wav(Path::new(wav_path))?;
        let mut decoder =
            Ft8Decoder::new(config.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        scored += 1;
        for d in &decoded {
            if truth.contains(&hash_normalize_message(&d.text)) {
                tps += 1;
            } else {
                fps += 1;
            }
        }
    }
    Ok((tps, fps, t0.elapsed().as_secs_f64(), scored))
}

// ---------------------------------------------------------------------------
// Part: instrument — unsatisfied-check distribution, gate disabled
// ---------------------------------------------------------------------------

fn part_instrument(n_wavs: usize) -> Result<()> {
    println!("## Instrumentation — hard_200 first {n_wavs} WAVs, iters=2, gate OFF (max_unsat=83)");
    let ws = workspace_root()?;
    let instr_path = std::env::temp_dir().join(format!("batch98_instr_{}.log", std::process::id()));
    let _ = std::fs::remove_file(&instr_path);
    // Must be set before the first decode initializes the OnceLock sink.
    std::env::set_var("PANCETTA_BICM_ID_INSTRUMENT_FILE", instr_path.as_os_str());

    let entries = manifest_entries(&ws, "hard_200.manifest.json")?;
    let entries: Vec<_> = entries.into_iter().take(n_wavs).collect();
    let config = cfg(2, 83);

    // Histograms over unsat 0..=83: (a) ok-true, (b) ok-fp, (c) fail,
    // plus the internally rejected rescues.
    let mut hist_true = [0usize; 84];
    let mut hist_fp = [0usize; 84];
    let mut hist_fail = [0usize; 84];
    let mut hist_reject = [0usize; 84];
    let mut offset = 0u64;

    for (wav_path, sha) in &entries {
        let Some(truth) = load_ft8lib_truth(&ws, sha) else {
            continue;
        };
        let samples = load_wav(Path::new(wav_path))?;
        let mut decoder =
            Ft8Decoder::new(config.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        // Classify this WAV's new instrumentation lines with its truth.
        let Ok(mut f) = std::fs::File::open(&instr_path) else {
            continue; // no rescues fired yet anywhere
        };
        f.seek(std::io::SeekFrom::Start(offset))?;
        let mut consumed = 0u64;
        for line in std::io::BufReader::new(&mut f).lines() {
            let line = line?;
            consumed += line.len() as u64 + 1;
            let mut it = line.splitn(3, ' ');
            let (Some(kind), Some(unsat)) = (it.next(), it.next()) else {
                continue;
            };
            let Ok(unsat) = unsat.parse::<usize>() else {
                continue;
            };
            let unsat = unsat.min(83);
            match kind {
                "ok" => {
                    let text = it.next().unwrap_or("");
                    if truth.contains(&hash_normalize_message(text)) {
                        hist_true[unsat] += 1;
                    } else {
                        hist_fp[unsat] += 1;
                    }
                }
                "fail" => hist_fail[unsat] += 1,
                "reject" => hist_reject[unsat] += 1,
                _ => {}
            }
        }
        offset += consumed;
    }
    let _ = std::fs::remove_file(&instr_path);

    let tot = |h: &[usize; 84]| h.iter().sum::<usize>();
    let (nt, nf, nl, nr) = (
        tot(&hist_true),
        tot(&hist_fp),
        tot(&hist_fail),
        tot(&hist_reject),
    );
    println!("\n  totals: ok-true={nt} ok-fp={nf} fail={nl} reject={nr}");
    println!("\n  unsat | ok-true | ok-fp | reject | fail");
    println!("  ----- | ------- | ----- | ------ | ----");
    for u in 0..84 {
        if hist_true[u] + hist_fp[u] + hist_fail[u] + hist_reject[u] > 0 {
            println!(
                "  {u:>5} | {:>7} | {:>5} | {:>6} | {:>4}",
                hist_true[u], hist_fp[u], hist_reject[u], hist_fail[u]
            );
        }
    }

    println!("\n  threshold T (keep rescues with unsat <= T): cumulative keep");
    println!("  T  | true kept       | fp kept         | fail kept");
    let mut ct = 0usize;
    let mut cf = 0usize;
    let mut cl = 0usize;
    let mut recommended: Option<usize> = None;
    for t in 0..84 {
        ct += hist_true[t];
        cf += hist_fp[t];
        cl += hist_fail[t];
        if hist_true[t] + hist_fp[t] + hist_fail[t] > 0 || t < 40 {
            let pt = if nt > 0 {
                100.0 * ct as f64 / nt as f64
            } else {
                0.0
            };
            let pf = if nf > 0 {
                100.0 * cf as f64 / nf as f64
            } else {
                0.0
            };
            let pl = if nl > 0 {
                100.0 * cl as f64 / nl as f64
            } else {
                0.0
            };
            if hist_true[t] + hist_fp[t] > 0 {
                println!(
                    "  {t:>2} | {ct:>4}/{nt:<4} {pt:>5.1}% | {cf:>4}/{nf:<4} {pf:>5.1}% | {cl}/{nl} {pl:.1}%"
                );
            }
        }
        if recommended.is_none() && nt > 0 && ct as f64 >= 0.8 * nt as f64 {
            recommended = Some(t);
        }
    }
    match recommended {
        Some(t) => println!("\n  recommended threshold (smallest T keeping >=80% of ok-true): {t}"),
        None => println!("\n  no ok-true rescues observed — threshold must come from judgment"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Part: spot — hard_200/50 at the gated config
// ---------------------------------------------------------------------------

fn part_spot(n_wavs: usize, max_unsat: usize) -> Result<()> {
    println!("## Gated spot — hard_200 first {n_wavs} WAVs, iters {{0, 2}}, max_unsat={max_unsat}");
    let ws = workspace_root()?;
    let entries = manifest_entries(&ws, "hard_200.manifest.json")?;
    let entries: Vec<_> = entries.into_iter().take(n_wavs).collect();
    let mut base: Option<(usize, usize, f64)> = None;
    for iters in [0usize, 2] {
        let (tp, fp, wall, scored) = run_corpus(&ws, &entries, &cfg(iters, max_unsat))?;
        println!("  iters={iters}: TP={tp} FP={fp} wall={wall:.1}s ({scored} WAVs scored)");
        match base {
            None => base = Some((tp, fp, wall)),
            Some((tp0, fp0, w0)) => {
                let dtp = tp as i64 - tp0 as i64;
                let dfp = fp as i64 - fp0 as i64;
                println!(
                    "  ΔTP={dtp:+} ΔFP={dfp:+} wall {:+.1}%  (proceed bar: ΔFP ≤ 2×ΔTP, ΔTP > 0)",
                    100.0 * (wall - w0) / w0
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Part: synth — quick gated SNR sweep
// ---------------------------------------------------------------------------

fn part_synth(trials: usize, max_unsat: usize) -> Result<()> {
    println!(
        "## Synthetic re-check — paired AWGN, {trials} trials/point, iters {{0, 2}}, max_unsat={max_unsat}"
    );
    let mut encoder = Ft8Encoder::new();
    let mut modulator =
        Ft8Modulator::new_default().map_err(|e| anyhow::anyhow!("modulator: {e}"))?;
    let mut cleans: Vec<Vec<f32>> = Vec::with_capacity(MESSAGES.len());
    for msg in MESSAGES {
        let symbols = encoder
            .encode_message(msg, None)
            .map_err(|e| anyhow::anyhow!("encode '{msg}': {e}"))?;
        let mut clean = modulator
            .modulate_symbols(&symbols, 0.0)
            .map_err(|e| anyhow::anyhow!("modulate '{msg}': {e}"))?;
        clean.resize(WINDOW_SAMPLES, 0.0);
        cleans.push(clean);
    }

    let snrs: Vec<f32> = (0..17).map(|k| -24.0 + 0.5 * k as f32).collect();
    let iter_configs: [usize; 2] = [0, 2];
    let mut rates: Vec<Vec<f64>> = vec![Vec::new(); iter_configs.len()];
    let mut walls: Vec<f64> = vec![0.0; iter_configs.len()];

    println!("\n  SNR (dB) | iters=0        | iters=2 gated");
    println!("  -------- | -------------- | --------------");
    for (si, &snr_db) in snrs.iter().enumerate() {
        let mut successes = [0usize; 2];
        for trial in 0..trials {
            let msg_idx = trial % MESSAGES.len();
            let clean = &cleans[msg_idx];
            let p_signal = signal_power(clean);
            let sigma = sigma_for_snr_db(p_signal, snr_db);
            // Same seed family as batch97 for comparability.
            let seed = 0xB97_0000u64 + (si as u64) * 1000 + trial as u64;
            let mut rng = StdRng::seed_from_u64(seed);
            let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
            let recv: Vec<f32> = clean.iter().zip(&noise).map(|(s, n)| s + n).collect();
            for (ci, &iters) in iter_configs.iter().enumerate() {
                let t0 = std::time::Instant::now();
                let mut decoder = Ft8Decoder::new(cfg(iters, max_unsat))
                    .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
                let decoded = decoder
                    .decode_window(&recv)
                    .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
                walls[ci] += t0.elapsed().as_secs_f64();
                if decoded.iter().any(|d| d.text == MESSAGES[msg_idx]) {
                    successes[ci] += 1;
                }
            }
        }
        let mut row = format!("  {snr_db:>+6.1}  ");
        for (ci, &s) in successes.iter().enumerate() {
            let rate = s as f64 / trials as f64;
            rates[ci].push(rate);
            row.push_str(&format!("| {s:>3}/{trials:<3} {:>5.1}% ", rate * 100.0));
        }
        println!("{row}");
    }

    println!("\n### 50%-decode-rate thresholds");
    let t0 = threshold_50(&snrs, &rates[0]);
    let t2 = threshold_50(&snrs, &rates[1]);
    for (thr, label, wall) in [(t0, "iters=0", walls[0]), (t2, "iters=2 gated", walls[1])] {
        match thr {
            Some(t) => println!("  {label}: {t:+.2} dB   (decode wall {wall:.1}s)"),
            None => println!("  {label}: never reaches 50% (wall {wall:.1}s)"),
        }
    }
    if let (Some(a), Some(b)) = (t0, t2) {
        println!(
            "\n  gated threshold shift 0→2: {:+.3} dB  (bar: ≥ 0.2 dB)",
            a - b
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Part: full — graduation corpora
// ---------------------------------------------------------------------------

fn part_full(max_unsat: usize, manifest_name: &str) -> Result<()> {
    println!("## Full corpus — {manifest_name}, iters {{0, 2}}, max_unsat={max_unsat}");
    let ws = workspace_root()?;
    let entries = manifest_entries(&ws, manifest_name)?;
    println!("  {} manifest entries", entries.len());
    let mut base: Option<(usize, usize, f64)> = None;
    for iters in [0usize, 2] {
        let (tp, fp, wall, scored) = run_corpus(&ws, &entries, &cfg(iters, max_unsat))?;
        println!("  iters={iters}: TP={tp} FP={fp} wall={wall:.1}s ({scored} WAVs scored)");
        match base {
            None => base = Some((tp, fp, wall)),
            Some((tp0, fp0, w0)) => {
                let dtp = tp as i64 - tp0 as i64;
                let dfp = fp as i64 - fp0 as i64;
                println!(
                    "  ΔTP={dtp:+} ΔFP={dfp:+} wall {:+.1}%",
                    100.0 * (wall - w0) / w0
                );
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let part = std::env::var("BATCH98_PART").unwrap_or_else(|_| "spot".to_string());
    let n_wavs: usize = std::env::var("BATCH98_WAVS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let trials: usize = std::env::var("BATCH98_TRIALS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let max_unsat: usize = std::env::var("BATCH98_MAX_UNSAT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| Ft8Config::default().bicm_id_max_unsatisfied_checks);
    let corpus = std::env::var("BATCH98_CORPUS")
        .unwrap_or_else(|_| "raw_530_full.manifest.json".to_string());

    println!("# Batch 98 — hb-252 BICM-ID gated graduation run\n");
    match part.as_str() {
        "instrument" => part_instrument(n_wavs)?,
        "spot" => part_spot(n_wavs, max_unsat)?,
        "synth" => part_synth(trials, max_unsat)?,
        "full" => part_full(max_unsat, &corpus)?,
        other => anyhow::bail!("unknown BATCH98_PART '{other}'"),
    }
    Ok(())
}
