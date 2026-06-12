//! Batch 101 — hb-256 impulse-robust per-symbol LLR weighting kill-switch.
//!
//! Mechanism (translated): Clavier, Peters, Septier & Nevat (EURASIP
//! JWCN 2021, eq. (15)) show that under impulsive (alpha-stable /
//! sub-exponential) noise the optimal LLR is non-monotonic —
//! `LLR(y) = sign(y)·min(a|y|, b/|y|)` — large received amplitudes
//! must be ATTENUATED, not trusted. Their `y` is a time-domain
//! matched-filter output; pancetta's LLRs are dB tone-power
//! differences in the spectrogram domain with no per-bit scalar
//! amplitude, so the literal form does not map. The faithful
//! translation (implemented behind `Ft8Config::impulse_robust_llr`):
//! a lightning crash is broadband + short-time, inflating ALL tone
//! bins of 1-3 symbols, so the per-symbol total tone power is the
//! amplitude statistic analogous to |y|; symbols whose total power
//! exceeds `k×` the candidate's median symbol power get their LLRs
//! multiplied by `w = k·P_med/P_s` (the inverse branch), others are
//! untouched (the linear branch).
//!
//! Part A (synthetic SNR sweep, harness pattern from
//! `batch97/99/100`): decode rate for configs {off, k=3, k=6} ×
//! noise {pure AWGN, impulsive}, SNR −24..−16 dB in 0.5 dB steps
//! (2500 Hz reference BW, measured against the BASE AWGN floor in
//! both noise models — impulses are additional energy). Reports
//! 50%-decode-rate thresholds per noise type.
//!
//! Impulsive-noise plant (documented model — Bernoulli-timed
//! broadband bursts, the simple alternative to a symmetric
//! alpha-stable sampler):
//!   - base: AWGN at σ for the target SNR (identical to the pure-AWGN
//!     plant, paired seeds);
//!   - burst process: at each FT8 symbol boundary (1920 samples @
//!     12 kHz), with probability p = 0.02 a burst begins; duration
//!     uniform in {1, 2, 3} symbols; bursts may overlap (additive);
//!   - burst waveform: extra white Gaussian noise with σ_burst =
//!     σ_base · 10^(A/20), A ~ Uniform(10, 30) dB — broadband and
//!     10-30 dB above the noise floor, like lightning static;
//!   - expected duty cycle ≈ p · E[dur] = 4% of symbols impacted.
//!
//! Pre-registered bars (see
//! `research/notes/2026-06-12-batch101-impulse-llr.md`):
//!   (a) pure AWGN: robust-on must NOT regress — threshold regression
//!       > 0.05 dB vs off = FAIL;
//!   (b) impulsive: robust-on threshold improvement ≥ +0.3 dB = PASS.
//!
//! Part B (real-corpus spot): hard_200 first 50 WAVs, {off, k=6},
//! hash-normalized ft8_lib-truth scoring. Bar: ΔTP ≥ −2 (no
//! real-corpus regression; the curated corpora have no known storm
//! days, so this is a do-no-harm check, not a gain check).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch101_impulse_robust_llr_kill_switch
//! Env:
//!   BATCH101_TRIALS=50      trials per SNR point (Part A)
//!   BATCH101_REAL_WAVS=50   WAV count (Part B)
//!   BATCH101_SKIP_SYNTH=1 / BATCH101_SKIP_REAL=1

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES};
use pancetta_research::metrics::hash_normalize_message;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: f32 = 12_000.0;
/// FT8 symbol period in samples at 12 kHz (1920 = 160 ms).
const SYMBOL_SAMPLES: usize = 1920;

/// 20 distinct standard messages (CQ / grid / report / RR73 shapes).
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

/// Probe configs: attenuation knee (None = off, byte-identical).
const CONFIGS: [(Option<f64>, &str); 3] = [(None, "off"), (Some(3.0), "k=3"), (Some(6.0), "k=6")];

/// Noise plants.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NoisePlant {
    PureAwgn,
    Impulsive,
}

impl NoisePlant {
    fn name(self) -> &'static str {
        match self {
            NoisePlant::PureAwgn => "pure AWGN",
            NoisePlant::Impulsive => "impulsive",
        }
    }
}

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

/// Bernoulli-timed broadband impulse bursts added IN PLACE on top of
/// the base AWGN. See module docs for the model spec.
fn add_impulse_bursts(rng: &mut StdRng, noise: &mut [f32], sigma_base: f32) {
    let n_symbols = noise.len().div_ceil(SYMBOL_SAMPLES);
    // Per-symbol extra-noise σ accumulated from all active bursts (in
    // power, since independent Gaussian components add in variance).
    let mut extra_var = vec![0.0f64; n_symbols];
    for s in 0..n_symbols {
        if rng.gen_range(0.0f64..1.0) < 0.02 {
            let dur = rng.gen_range(1usize..=3);
            let amp_db = rng.gen_range(10.0f64..30.0);
            let sigma_burst = sigma_base as f64 * 10f64.powf(amp_db / 20.0);
            for d in 0..dur {
                if s + d < n_symbols {
                    extra_var[s + d] += sigma_burst * sigma_burst;
                }
            }
        }
    }
    for (s, &var) in extra_var.iter().enumerate() {
        if var <= 0.0 {
            continue;
        }
        let sigma = (var as f32).sqrt();
        let start = s * SYMBOL_SAMPLES;
        let end = ((s + 1) * SYMBOL_SAMPLES).min(noise.len());
        let burst = gaussian_noise(rng, end - start, sigma);
        for (x, b) in noise[start..end].iter_mut().zip(burst) {
            *x += b;
        }
    }
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

fn cfg_for(knee: Option<f64>) -> Ft8Config {
    Ft8Config {
        impulse_robust_llr: knee,
        ..Ft8Config::default()
    }
}

/// Linear-interpolated SNR at which the rate curve first crosses 50%
/// scanning from the lowest SNR upward. None if never ≥ 50%.
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

fn part_a_synthetic(trials: usize) -> Result<()> {
    println!("## Part A — synthetic SNR sweep (paired noise, {trials} trials/point)");

    // Pre-modulate the 20 clean reference signals once.
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
    let mut thresholds: Vec<Vec<Option<f64>>> = Vec::new(); // [plant][config]

    for &plant in &[NoisePlant::PureAwgn, NoisePlant::Impulsive] {
        println!("\n### Noise model: {}", plant.name());
        let mut rates: Vec<Vec<f64>> = vec![Vec::new(); CONFIGS.len()];
        let mut walls: Vec<f64> = vec![0.0; CONFIGS.len()];

        let header: String = CONFIGS
            .iter()
            .map(|(_, name)| format!("| {name:<14} "))
            .collect();
        println!("\n  SNR (dB) {header}");
        println!("  -------- {}", "| -------------- ".repeat(CONFIGS.len()));
        for (si, &snr_db) in snrs.iter().enumerate() {
            let mut successes = vec![0usize; CONFIGS.len()];
            for trial in 0..trials {
                let msg_idx = trial % MESSAGES.len();
                let clean = &cleans[msg_idx];
                let p_signal = signal_power(clean);
                let sigma = sigma_for_snr_db(p_signal, snr_db);
                // Paired noise: same base realization for all configs;
                // the impulsive plant ALSO shares the base AWGN seed
                // with the pure plant (the burst process consumes RNG
                // state after the base noise is drawn, so the base is
                // identical across plants at the same (si, trial)).
                let seed = 0xB101_0000u64 + (si as u64) * 1000 + trial as u64;
                let mut rng = StdRng::seed_from_u64(seed);
                let mut noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
                if plant == NoisePlant::Impulsive {
                    add_impulse_bursts(&mut rng, &mut noise, sigma);
                }
                let recv: Vec<f32> = clean.iter().zip(&noise).map(|(s, n)| s + n).collect();
                for (ci, &(knee, _)) in CONFIGS.iter().enumerate() {
                    let t0 = std::time::Instant::now();
                    let mut decoder = Ft8Decoder::new(cfg_for(knee))
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

        println!("\n#### 50%-decode-rate thresholds ({})", plant.name());
        let mut thrs: Vec<Option<f64>> = Vec::new();
        for (ci, &(_, name)) in CONFIGS.iter().enumerate() {
            let thr = threshold_50(&snrs, &rates[ci]);
            match thr {
                Some(t) => println!("  {name:<6}: {t:+.2} dB   (decode wall {:.1}s)", walls[ci]),
                None => println!(
                    "  {name:<6}: curve never reaches 50% in [-24,-16] (wall {:.1}s)",
                    walls[ci]
                ),
            }
            thrs.push(thr);
        }
        if let Some(off) = thrs[0] {
            for (ci, &(_, name)) in CONFIGS.iter().enumerate().skip(1) {
                if let Some(t) = thrs[ci] {
                    println!("  shift off → {name}: {:+.3} dB", off - t);
                }
            }
        }
        thresholds.push(thrs);
    }

    println!("\n### Pre-registered bars");
    let awgn = &thresholds[0];
    let imp = &thresholds[1];
    if let Some(off_awgn) = awgn[0] {
        for (ci, &(_, name)) in CONFIGS.iter().enumerate().skip(1) {
            if let Some(t) = awgn[ci] {
                let shift = off_awgn - t; // positive = improvement
                let ok = shift >= -0.05;
                println!(
                    "  (a) AWGN no-regression {name}: {shift:+.3} dB → {}",
                    if ok {
                        "PASS (≥ −0.05)"
                    } else {
                        "FAIL (regression > 0.05 dB)"
                    }
                );
            }
        }
    }
    if let Some(off_imp) = imp[0] {
        for (ci, &(_, name)) in CONFIGS.iter().enumerate().skip(1) {
            if let Some(t) = imp[ci] {
                let shift = off_imp - t;
                let ok = shift >= 0.3;
                println!(
                    "  (b) impulsive improvement {name}: {shift:+.3} dB → {}",
                    if ok {
                        "PASS (≥ +0.3)"
                    } else {
                        "FAIL (< +0.3)"
                    }
                );
            }
        }
    }
    Ok(())
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

fn part_b_real(n_wavs: usize) -> Result<()> {
    println!("\n## Part B — real-corpus spot check (hard_200 first {n_wavs} WAVs, {{off, k=6}})");
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries: Vec<&Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .take(n_wavs)
        .collect();

    let spot_configs: [(Option<f64>, &str); 2] = [(None, "off"), (Some(6.0), "k=6")];
    let mut missing_truth = 0usize;
    let mut results: Vec<(usize, usize)> = Vec::new();
    for (ci, &(knee, name)) in spot_configs.iter().enumerate() {
        let cfg = cfg_for(knee);
        let mut tps = 0usize;
        let mut fps = 0usize;
        let t0 = std::time::Instant::now();
        for entry in &entries {
            let wav_path = entry["wav_path"].as_str().context("wav_path")?;
            let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
            let Some(truth) = load_ft8lib_truth(&ws, sha) else {
                if ci == 0 {
                    missing_truth += 1;
                }
                continue;
            };
            let samples = load_wav(Path::new(wav_path))?;
            let mut decoder = Ft8Decoder::new(cfg.clone())
                .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
            let decoded = decoder
                .decode_window(&samples)
                .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
            for d in &decoded {
                if truth.contains(&hash_normalize_message(&d.text)) {
                    tps += 1;
                } else {
                    fps += 1;
                }
            }
        }
        let wall = t0.elapsed().as_secs_f64();
        println!("  {name:<6}: TP={tps} FP={fps} wall={wall:.1}s");
        results.push((tps, fps));
    }
    if missing_truth > 0 {
        println!("  ({missing_truth} WAVs skipped — no ft8_lib truth file)");
    }
    if results.len() == 2 {
        let (tp_off, fp_off) = results[0];
        let (tp_on, fp_on) = results[1];
        let dtp = tp_on as i64 - tp_off as i64;
        let dfp = fp_on as i64 - fp_off as i64;
        println!("\n### Delta (k=6 vs off): ΔTP={dtp:+} ΔFP={dfp:+}");
        println!(
            "  real-corpus no-regression bar (ΔTP ≥ −2): {}",
            if dtp >= -2 { "PASS" } else { "FAIL" }
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let trials: usize = std::env::var("BATCH101_TRIALS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let n_wavs: usize = std::env::var("BATCH101_REAL_WAVS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    println!("# Batch 101 — hb-256 impulse-robust per-symbol LLR weighting kill-switch\n");
    if std::env::var("BATCH101_SKIP_SYNTH").is_err() {
        part_a_synthetic(trials)?;
    }
    if std::env::var("BATCH101_SKIP_REAL").is_err() {
        part_b_real(n_wavs)?;
    }
    Ok(())
}
