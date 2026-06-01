//! hb-089 — multi-cycle coherent residual accumulation, kill-switch diagnostic.
//!
//! Hypothesis: after hb-079 multipass saturates, the post-subtract residual
//! still contains weak-signal energy at the truth coordinates of missed
//! truths. Averaging the residual spectrogram across N=3-5 same-slot
//! sub-windows (start offsets 0.0s, 0.5s, 1.0s, ... — Welch-style overlapping
//! STFT views) should pull the truth out of the noise floor by ≥2 dB.
//!
//! ## Corpus structural finding (CRITICAL)
//!
//! pancetta's curated hard-200 corpus uses 15-second WAVs (one FT8 slot each).
//! The bank-entry's kill-switch ("for missed truths whose callsign appears in
//! 2+ same-slot sub-windows") presumes ≥30s WAVs with cross-slot repeats.
//! That condition is NOT satisfiable in this corpus — no callsign repeats
//! within a 15s slot. So the BANK-stated kill switch yields 0 eligible
//! truths (= unconditional KILL).
//!
//! This diagnostic falls back to the TASK-message's alternative
//! interpretation: power-Welch averaging of overlapping shifted sub-windows
//! of the SAME residual audio (start offsets 0, +0.25s, +0.50s; each ≥12.64s
//! long so the FT8 message span fits). For correlated-noise (high overlap),
//! the theoretical SNR improvement bound is ~10·log10(T_total / T_window)
//! per Welch — at 15 s total / 13 s window that's ~0.6 dB best case.
//!
//! ## What we measure
//!
//! For each top-5 hard-200 worst WAV:
//!   1. Decode with production `Ft8Decoder` (full multipass / joint-pair).
//!   2. For each production decode that retains `tone_symbols`,
//!      re-modulate the signal at its decoded frequency and time offset,
//!      subtract it from the time-domain audio (amplitude-fit by Costas
//!      energy match). This gives a "residual audio" that approximates what
//!      hb-079's complex subtraction produces in the spectrogram domain.
//!   3. For each MISSED truth (truth not in production decodes), compute
//!      SNR at the truth's (freq_hz, dt_s) coordinates on:
//!        (a) single-window spectrogram of the residual (control)
//!        (b) 3-sub-window Welch-averaged power spectrogram of the residual
//!            with start offsets {0, 0.25s, 0.50s}
//!   4. Report mean SNR delta (b − a). PROCEED if ≥ 2 dB.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb089_residual_accumulation_diagnostic

use anyhow::Context;
use num_complex::Complex;
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, ProtocolParams};
use rustfft::FftPlanner;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

const SAMPLE_RATE: u32 = 12_000;
const FREQ_OSR: usize = 2;
const TIME_OSR: usize = 2;
const NUM_TONES: usize = 8;
const NUM_SYMBOLS: usize = 79;
const SLOT_S: f64 = 15.0;
const TOP_K_WAVS: usize = 5;
const SNR_DELTA_THRESHOLD_DB: f64 = 2.0;

// Welch sub-window starting offsets, in samples (12 kHz).
// Each sub-window covers `SUBWINDOW_SAMPLES` of audio starting at this offset.
// With raw audio of 15.0 s = 180_000 samples and sub-window length 156_000
// (= 13.0 s), the legal start range is 0..=24_000 samples (= 0..2.0 s).
// We pick {0, 3000, 6000} = {0.00 s, 0.25 s, 0.50 s} — modest overlap but
// enough to test the Welch claim.
const SUBWINDOW_SAMPLES: usize = 156_000;
const SUBWINDOW_OFFSETS: [usize; 3] = [0, 3_000, 6_000];

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_wav(path: &PathBuf) -> anyhow::Result<Vec<f32>> {
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

// ============================================================================
// Spectrogram (matches Ft8Decoder::compute_spectrogram for FT8 @ 12 kHz)
// ============================================================================

struct Spec {
    /// power[time_step][freq_sub][freq_bin] = magnitude in dB
    power: Vec<Vec<Vec<f64>>>,
    num_steps: usize,
    num_bins: usize,
}

fn compute_spectrogram(audio: &[f64]) -> Spec {
    let block_size = 1920; // FT8 samples-per-symbol @ 12 kHz
    let nfft = block_size * FREQ_OSR;
    let subblock_size = block_size / TIME_OSR;
    let num_bins = block_size / 2 + 1;

    let msg_span = NUM_SYMBOLS * TIME_OSR;
    let min_steps = msg_span + 50;
    let min_blocks = (min_steps + TIME_OSR - 1) / TIME_OSR;

    let mut audio_v: Vec<f64> = audio.to_vec();
    let num_blocks = audio_v.len() / block_size;
    if num_blocks < min_blocks {
        audio_v.resize(min_blocks * block_size, 0.0);
    }
    let num_blocks = audio_v.len() / block_size;
    let num_steps = num_blocks * TIME_OSR;

    let fft_norm = 2.0 / nfft as f64;
    let window: Vec<f64> = (0..nfft)
        .map(|i| {
            let x = (std::f64::consts::PI * i as f64 / nfft as f64).sin();
            fft_norm * x * x
        })
        .collect();

    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(nfft);

    let mut power = Vec::with_capacity(num_steps);
    let mut fft_buf = vec![Complex::new(0.0, 0.0); nfft];
    let mut last_frame = vec![0.0f64; nfft];
    let mut frame_pos = 0usize;

    for _block in 0..num_blocks {
        for _time_sub in 0..TIME_OSR {
            last_frame.copy_within(subblock_size.., 0);
            let new_start = nfft - subblock_size;
            for pos in 0..subblock_size {
                last_frame[new_start + pos] = if frame_pos < audio_v.len() {
                    audio_v[frame_pos]
                } else {
                    0.0
                };
                frame_pos += 1;
            }
            for i in 0..nfft {
                fft_buf[i] = Complex::new(window[i] * last_frame[i], 0.0);
            }
            fft.process(&mut fft_buf);
            let mut sub_power: Vec<Vec<f64>> = Vec::with_capacity(FREQ_OSR);
            for fs in 0..FREQ_OSR {
                let mut row = Vec::with_capacity(num_bins);
                for bin in 0..num_bins {
                    let src_bin = bin * FREQ_OSR + fs;
                    if src_bin < nfft / 2 + 1 {
                        let mag2 = fft_buf[src_bin].norm_sqr();
                        let db = 10.0 * (1e-12_f64 + mag2).log10();
                        row.push(db);
                    } else {
                        row.push(-120.0);
                    }
                }
                sub_power.push(row);
            }
            power.push(sub_power);
        }
    }

    Spec {
        power,
        num_steps,
        num_bins,
    }
}

/// Welch-average the LINEAR-power of `n` sub-windows of `audio` at the given
/// sample-offsets, each of length `SUBWINDOW_SAMPLES`. Returns a Spec in dB,
/// computed by averaging linear power per (time_step, fs, freq_bin) cell.
/// All sub-windows are aligned to the AUDIO sample-0 by padding the prefix
/// with zeros — so a peak at audio time t shows up at the SAME (time_step)
/// in every sub-window's spectrogram. (This is the residual-coherent-average
/// approximation: noise samples differ by the offset; signal samples line up.)
fn welch_average_spectrogram(audio: &[f64], offsets: &[usize]) -> Spec {
    let specs: Vec<Spec> = offsets
        .iter()
        .map(|&off| {
            // Pad the front with zeros so sample t in the sub-window
            // corresponds to sample (off + t) in the original audio.
            // This way the absolute sample positions of all subwindows line up.
            // Equivalently: take the suffix audio[off..off+SUBWINDOW_SAMPLES],
            // then prepend `off` zeros.
            let end = (off + SUBWINDOW_SAMPLES).min(audio.len());
            let mut sub = vec![0.0f64; off];
            sub.extend_from_slice(&audio[off..end]);
            // Pad the back so all sub-windows are equal length and the
            // spectrogram has the same time-step count.
            if sub.len() < audio.len() {
                sub.resize(audio.len(), 0.0);
            }
            compute_spectrogram(&sub)
        })
        .collect();

    // Use the smallest num_steps across all subs (they should be equal but
    // guard anyway).
    let num_steps = specs.iter().map(|s| s.num_steps).min().unwrap_or(0);
    let num_bins = specs[0].num_bins;

    let mut power = Vec::with_capacity(num_steps);
    for t in 0..num_steps {
        let mut sub_power: Vec<Vec<f64>> = Vec::with_capacity(FREQ_OSR);
        for fs in 0..FREQ_OSR {
            let mut row = Vec::with_capacity(num_bins);
            for bin in 0..num_bins {
                // Average LINEAR power: db -> linear -> mean -> db.
                let mut lin_sum = 0.0f64;
                let n = specs.len() as f64;
                for s in &specs {
                    let db = s.power[t][fs][bin];
                    lin_sum += 10f64.powf(db / 10.0);
                }
                let mean_lin = lin_sum / n;
                let mean_db = 10.0 * (mean_lin + 1e-30).log10();
                row.push(mean_db);
            }
            sub_power.push(row);
        }
        power.push(sub_power);
    }
    Spec {
        power,
        num_steps,
        num_bins,
    }
}

/// Estimate SNR at the truth's (freq_hz, dt_s) coordinates by:
///   1. Mapping (freq_hz, dt_s) to (freq_bin, time_step, freq_sub).
///   2. Encoding the truth message to get its 79 tone symbols.
///   3. Computing the signal power as the mean dB at the expected
///      (symbol, expected_tone) cells across all 79 symbols.
///   4. Computing the noise floor as the mean dB across the 7
///      NON-expected tones per symbol (clean noise estimate when the
///      signal is correctly localised to one tone per symbol).
///   5. SNR_dB = signal_dB − noise_dB.
fn snr_at_truth(
    spec: &Spec,
    truth_text: &str,
    freq_hz: f64,
    dt_s: f64,
    pp: &ProtocolParams,
) -> Option<f64> {
    // Re-encode truth text to recover its 79 tone-symbols.
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder.encode_message(truth_text, None).ok()?;
    if symbols.len() < NUM_SYMBOLS {
        return None;
    }

    // Map (freq_hz, dt_s) to (freq_bin, freq_sub, time_step).
    let tone_spacing = pp.tone_spacing;
    let bin_size = tone_spacing / FREQ_OSR as f64; // 3.125 Hz
    let bin_idx = (freq_hz / bin_size).round() as isize;
    if bin_idx < 0 {
        return None;
    }
    let freq_bin = (bin_idx / FREQ_OSR as isize) as usize;
    let freq_sub = (bin_idx as usize) % FREQ_OSR;

    // hb088 example shows time_step computation. Spectrogram has
    // time_padding = 0 in our re-implementation. Each time_step = 0.08 s
    // (subblock_size/SAMPLE_RATE = 960/12000).
    let step_seconds = 0.08_f64;
    let t0_signed = (dt_s / step_seconds).round() as isize;
    if t0_signed < 0 {
        return None;
    }
    let t0 = t0_signed as usize;

    let steps_per_symbol = TIME_OSR;
    let mut signal_dbs = Vec::with_capacity(NUM_SYMBOLS);
    let mut noise_dbs = Vec::with_capacity(NUM_SYMBOLS * (NUM_TONES - 1));

    for sym_idx in 0..NUM_SYMBOLS {
        let expected_tone = symbols[sym_idx] as usize;
        let t_base = t0 + sym_idx * steps_per_symbol;
        if t_base + 1 >= spec.num_steps {
            continue;
        }
        for tone in 0..NUM_TONES {
            let fb = freq_bin + tone;
            if fb >= spec.num_bins {
                continue;
            }
            // Average two TIME_OSR slots like extract_symbols.
            let a = spec.power[t_base][freq_sub][fb];
            let b = spec.power[t_base + 1][freq_sub][fb];
            let avg = (a + b) * 0.5;
            if tone == expected_tone {
                signal_dbs.push(avg);
            } else {
                noise_dbs.push(avg);
            }
        }
    }
    if signal_dbs.is_empty() || noise_dbs.is_empty() {
        return None;
    }
    let mean_signal = signal_dbs.iter().sum::<f64>() / signal_dbs.len() as f64;
    let mean_noise = noise_dbs.iter().sum::<f64>() / noise_dbs.len() as f64;
    Some(mean_signal - mean_noise)
}

/// Subtract a decoded signal from time-domain audio by re-modulating it at
/// the decoded frequency and time offset, fitting amplitude via least squares
/// (i.e. ⟨residual, template⟩ / ⟨template, template⟩), and subtracting.
fn subtract_decode_time_domain(audio: &mut [f32], decode_text: &str, freq_hz: f64, dt_s: f64) {
    let mut encoder = Ft8Encoder::new();
    let Ok(symbols) = encoder.encode_message(decode_text, None) else {
        return;
    };
    let Ok(mut modulator) = Ft8Modulator::new(SAMPLE_RATE, freq_hz, 1.0) else {
        return;
    };
    let Ok(template) = modulator.modulate_symbols(&symbols, 0.0) else {
        return;
    };

    // Shift template by dt_s samples (positive dt → signal arrives later).
    let dt_samples = (dt_s * SAMPLE_RATE as f64).round() as isize;
    let n = audio.len();
    let mut shifted = vec![0.0f32; n];
    for (i, &t) in template.iter().enumerate() {
        let dst = i as isize + dt_samples;
        if dst >= 0 && (dst as usize) < n {
            shifted[dst as usize] = t;
        }
    }

    // Least-squares amplitude fit: alpha = <audio, template> / <template, template>.
    let mut num = 0.0_f64;
    let mut den = 0.0_f64;
    for i in 0..n {
        num += audio[i] as f64 * shifted[i] as f64;
        den += (shifted[i] as f64) * (shifted[i] as f64);
    }
    if den < 1e-30 {
        return;
    }
    let alpha = (num / den) as f32;
    for i in 0..n {
        audio[i] -= alpha * shifted[i];
    }
}

fn main() -> anyhow::Result<()> {
    let ws = workspace_root()?;
    let main_json: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/scorecards/main.json"),
    )?)?;
    let top_hashes: Vec<String> = main_json["tiers"]["curated-hard-200"]["per_wav_top_failures"]
        .as_array()
        .context("per_wav_top_failures missing")?
        .iter()
        .take(TOP_K_WAVS)
        .map(|f| f["wav_hash"].as_str().unwrap().to_string())
        .collect();

    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let mut path_by_sha: HashMap<String, String> = HashMap::new();
    for e in manifest["entries"].as_array().context("no entries")? {
        path_by_sha.insert(
            e["wav_sha256"].as_str().unwrap().to_string(),
            e["wav_path"].as_str().unwrap().to_string(),
        );
    }

    println!("hb-089 residual-accumulation diagnostic (top-{TOP_K_WAVS} hard-200)");
    println!("Sub-window offsets (samples): {:?}", SUBWINDOW_OFFSETS);
    println!("Sub-window length (samples): {SUBWINDOW_SAMPLES}");
    println!();
    println!(
        "{:>9} {:>6} {:>6} {:>6} {:>10} {:>10} {:>9}",
        "sha", "truth", "rec", "missed", "snr_base", "snr_welch", "Δ_dB"
    );
    println!("{}", "-".repeat(64));

    let pp = ProtocolParams::ft8();

    // Per-WAV aggregates
    let mut all_deltas: Vec<f64> = Vec::new();

    for sha in &top_hashes {
        let Some(wav_path_str) = path_by_sha.get(sha) else {
            continue;
        };
        let wav_path = PathBuf::from(wav_path_str);
        let Ok(samples) = load_wav(&wav_path) else {
            continue;
        };
        let baseline_path = ws.join(format!("research/baselines/ft8/{sha}.json"));
        let baseline: Value = serde_json::from_str(&std::fs::read_to_string(&baseline_path)?)?;
        let truths: Vec<(f64, f64, String)> = baseline["decodes"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|d| {
                        Some((
                            d.get("freq_hz")?.as_f64()?,
                            d.get("dt_s")?.as_f64()?,
                            d.get("message")?.as_str()?.trim().to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Production decode.
        let cfg = Ft8Config::default();
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let pancetta = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        // Build residual audio by subtracting each production decode's
        // re-modulated template (least-squares amplitude fit).
        let mut residual = samples.clone();
        for d in &pancetta {
            subtract_decode_time_domain(
                &mut residual,
                d.text.trim(),
                d.frequency_offset,
                d.time_offset,
            );
        }
        let residual_f64: Vec<f64> = residual.iter().map(|&s| s as f64).collect();

        // Classify recovered vs missed (substring match like the pair-density example).
        let pancetta_texts: Vec<String> =
            pancetta.iter().map(|m| m.text.trim().to_string()).collect();
        let mut missed = Vec::new();
        let mut recovered = 0usize;
        for (tf, td, tm) in &truths {
            let matched = pancetta_texts
                .iter()
                .any(|pm| pm.contains(tm) || tm.contains(pm));
            if matched {
                recovered += 1;
            } else {
                missed.push((*tf, *td, tm.clone()));
            }
        }

        // Build the single-window (control) and Welch-averaged spectrograms
        // of the residual.
        let spec_baseline = compute_spectrogram(&residual_f64);
        let spec_welch = welch_average_spectrogram(&residual_f64, &SUBWINDOW_OFFSETS);

        // Measure SNR at each missed-truth's coords on both.
        let mut wav_deltas: Vec<f64> = Vec::new();
        for (tf, td, tm) in &missed {
            // Some baseline-only truths may have dt outside [0, 15s] — clip.
            let dt = td.clamp(-2.0, SLOT_S);
            let base = match snr_at_truth(&spec_baseline, tm, *tf, dt, &pp) {
                Some(v) => v,
                None => continue,
            };
            let welch = match snr_at_truth(&spec_welch, tm, *tf, dt, &pp) {
                Some(v) => v,
                None => continue,
            };
            wav_deltas.push(welch - base);
        }
        let mean_base: f64;
        let mean_welch: f64;
        let mean_delta: f64;
        if wav_deltas.is_empty() {
            mean_base = f64::NAN;
            mean_welch = f64::NAN;
            mean_delta = f64::NAN;
        } else {
            // Re-collect raw values for reporting.
            let mut bases = Vec::with_capacity(missed.len());
            let mut welches = Vec::with_capacity(missed.len());
            for (tf, td, tm) in &missed {
                let dt = td.clamp(-2.0, SLOT_S);
                if let (Some(b), Some(w)) = (
                    snr_at_truth(&spec_baseline, tm, *tf, dt, &pp),
                    snr_at_truth(&spec_welch, tm, *tf, dt, &pp),
                ) {
                    bases.push(b);
                    welches.push(w);
                }
            }
            mean_base = bases.iter().sum::<f64>() / bases.len() as f64;
            mean_welch = welches.iter().sum::<f64>() / welches.len() as f64;
            mean_delta = wav_deltas.iter().sum::<f64>() / wav_deltas.len() as f64;
        }
        println!(
            "{:>9} {:>6} {:>6} {:>6} {:>10.2} {:>10.2} {:>9.3}",
            &sha[..8],
            truths.len(),
            recovered,
            missed.len(),
            mean_base,
            mean_welch,
            mean_delta,
        );
        all_deltas.extend(wav_deltas);
    }

    println!();
    println!("=== Aggregate ===");
    println!("Bank-stated kill switch (callsign in 2+ same-slot sub-windows): UNSATISFIABLE");
    println!("  Reason: hard-200 WAVs are 15.0s (one FT8 slot). No callsign repeats.");
    println!("  → that kill switch alone is sufficient to SHELVE hb-089.");
    println!();
    println!("Fallback: task-stated SNR-delta on Welch-averaged residual sub-windows:");
    if all_deltas.is_empty() {
        println!("  no eligible missed truths — SHELVE");
        return Ok(());
    }
    let n = all_deltas.len();
    let mean = all_deltas.iter().sum::<f64>() / n as f64;
    let mut sorted = all_deltas.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[n / 2];
    let p25 = sorted[n / 4];
    let p75 = sorted[3 * n / 4];

    println!("  Missed truths probed:   {n}");
    println!("  mean Δ SNR:             {mean:>7.3} dB");
    println!("  median Δ SNR:           {median:>7.3} dB");
    println!("  p25 / p75:              {p25:>7.3} / {p75:>7.3} dB");
    println!("  threshold (PROCEED):    ≥ {SNR_DELTA_THRESHOLD_DB} dB on the mean");
    let verdict = if mean >= SNR_DELTA_THRESHOLD_DB {
        "PROCEED — implement multi-cycle coherent residual accumulation"
    } else {
        "SHELVE — Welch averaging on residual sub-windows insufficient"
    };
    println!("  Verdict:                {verdict}");
    Ok(())
}
