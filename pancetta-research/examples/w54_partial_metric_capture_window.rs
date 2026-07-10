//! Task W5.4 (decoder-tp-sensitivity plan) [A/B]: retest
//! `Ft8Config::costas_partial_metric_enabled` "paired with W5.3"
//! (`extended_capture_window_enabled`).
//!
//! Why this needs a purpose-built synthetic harness instead of the
//! standard `eval`/`compare` hard-200 corpus, mirroring Task W5.3's own
//! finding (`w53_slot_edge_bucket_recall.rs`): `extended_capture_window_enabled`
//! is a `pancetta-config`/coordinator-level flag
//! (`coordinator/dsp.rs::boundary_anchored_slice`'s `lead_secs`), not a
//! `pancetta_ft8::Ft8Config` field — `pancetta-research`'s `decode_wav` calls
//! `Ft8Decoder` directly on pre-recorded WAVs and never touches that code
//! path, and the hard-200 WAV files themselves are exactly 15.0s captures
//! with no real "prior slot tail" audio to find a negative-dt signal in
//! regardless of what any decoder flag does. So this harness reproduces
//! W5.3's own rolling-capture-buffer + `boundary_anchored_slice` simulation,
//! adding `costas_partial_metric_enabled` as a second dimension — giving a
//! genuine (non-vacuous) measurement of whether the partial B+C Costas
//! metric's designed benefit (rescuing negative-dt candidates whose block A
//! falls outside the recorded window) is helped, redundant with, or
//! unaffected by a wider capture-window lead.
//!
//! `costas_partial_metric_enabled` IS a `pancetta_ft8::Ft8Config` field, so
//! its OWN A/B against the true production default is separately run on the
//! standard `eval`/`compare` hard-200 + noise_1000 corpora (see the W5.4
//! experiment log) — that measurement is real and decision-driving. THIS
//! harness is the secondary/diagnostic "paired with W5.3" measurement the
//! task brief explicitly asks for.
//!
//! Usage: `cargo run --release -p pancetta-research --example
//! w54_partial_metric_capture_window`

use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator};
use pancetta_research::synth::{add_awgn_2500hz_ref, signal_rms};
use std::time::Instant;

const SR: usize = 12_000;
const MSG_SAMPLES: usize = (SR as f64 * 12.64) as usize;
const DEFAULT_LEAD: f64 = 0.5;
const EXTENDED_LEAD: f64 = 1.0;
/// Same SNR chosen by W5.3 for the same reason: recall sits well below
/// 100% but well above the pure-capture-cliff floor, so the effect (if
/// any) is visible against a realistic backdrop.
const SNR_DB: f64 = -16.0;
const REPS_PER_DT: usize = 20;

/// Mirrors `coordinator/dsp.rs::boundary_anchored_slice` exactly (private
/// fn there) — copied digit-for-digit from W5.3's own verified copy.
fn boundary_anchored_slice(
    buffer_len: usize,
    emit_now_secs: f64,
    slot_boundary_secs: f64,
    sample_rate: usize,
    min_len: usize,
    lead_secs: f64,
) -> (usize, usize) {
    let secs_from_start = (emit_now_secs - slot_boundary_secs) + lead_secs;
    let samples_back = (secs_from_start * sample_rate as f64).round();
    let samples_back = if samples_back.is_finite() && samples_back > 0.0 {
        samples_back as usize
    } else {
        0
    };
    let len = samples_back.min(buffer_len).max(min_len.min(buffer_len));
    let start = buffer_len - len;
    (start, len)
}

/// One trial: embed a real modulated+noised "CQ K5ARH EM12" at true offset
/// `dt_secs` from the slot boundary in a rolling-capture buffer, slice via
/// `boundary_anchored_slice` with the given lead, decode with the given
/// `partial_enabled` config, report (hit/miss, elapsed decode time).
fn trial(
    dt_secs: f64,
    lead_secs: f64,
    partial_enabled: bool,
    seed: u64,
) -> (bool, std::time::Duration) {
    let mut enc = Ft8Encoder::new();
    let symbols = enc.encode_message("CQ K5ARH EM12", None).unwrap();
    let mut modu = Ft8Modulator::new_default().unwrap();
    let mut msg_audio = modu
        .modulate_symbols(&symbols, 500.0 + (seed % 50) as f64)
        .unwrap();
    let clean_rms = signal_rms(&msg_audio);
    add_awgn_2500hz_ref(&mut msg_audio, clean_rms, SNR_DB, seed);

    let boundary_secs = 10.0_f64;
    let emit_now_secs = boundary_secs + 13.0;
    let buffer_len = (emit_now_secs * SR as f64).round() as usize;
    let mut buffer = vec![0.0f32; buffer_len];
    add_awgn_2500hz_ref(&mut buffer, clean_rms, SNR_DB, seed.wrapping_add(999_983));
    let msg_start = ((boundary_secs + dt_secs) * SR as f64).round() as isize;
    for (i, &s) in msg_audio.iter().enumerate() {
        let idx = msg_start + i as isize;
        if idx >= 0 && (idx as usize) < buffer.len() {
            buffer[idx as usize] += s;
        }
    }

    let (start, len) = boundary_anchored_slice(
        buffer.len(),
        emit_now_secs,
        boundary_secs,
        SR,
        MSG_SAMPLES,
        lead_secs,
    );
    let window = &buffer[start..start + len];

    let config = Ft8Config {
        costas_partial_metric_enabled: partial_enabled,
        ..Ft8Config::default()
    };
    let mut dec = Ft8Decoder::new(config).unwrap();
    let t0 = Instant::now();
    let msgs = dec.decode_window(window).unwrap_or_default();
    let elapsed = t0.elapsed();
    (msgs.iter().any(|m| m.text.contains("K5ARH")), elapsed)
}

/// A pure-noise trial (no embedded message) at the given lead/partial
/// config — measures FP exposure the same way `noise_1000` does, but
/// specific to this synthetic harness's rolling-buffer construction.
fn noise_trial(lead_secs: f64, partial_enabled: bool, seed: u64) -> bool {
    let boundary_secs = 10.0_f64;
    let emit_now_secs = boundary_secs + 13.0;
    let buffer_len = (emit_now_secs * SR as f64).round() as usize;
    let mut buffer = vec![0.0f32; buffer_len];
    // Match the signal trials' noise floor (RMS calibrated against a
    // representative clean FT8 tone, not zero) so this is a fair
    // apples-to-apples FP check.
    let mut enc = Ft8Encoder::new();
    let symbols = enc.encode_message("CQ K5ARH EM12", None).unwrap();
    let mut modu = Ft8Modulator::new_default().unwrap();
    let ref_audio = modu.modulate_symbols(&symbols, 1500.0).unwrap();
    let clean_rms = signal_rms(&ref_audio);
    add_awgn_2500hz_ref(&mut buffer, clean_rms, SNR_DB, seed);

    let (start, len) = boundary_anchored_slice(
        buffer.len(),
        emit_now_secs,
        boundary_secs,
        SR,
        MSG_SAMPLES,
        lead_secs,
    );
    let window = &buffer[start..start + len];

    let config = Ft8Config {
        costas_partial_metric_enabled: partial_enabled,
        ..Ft8Config::default()
    };
    let mut dec = Ft8Decoder::new(config).unwrap();
    let msgs = dec.decode_window(window).unwrap_or_default();
    !msgs.is_empty()
}

fn main() {
    // Slot-edge-focused grid: the partial B+C metric's designed target is
    // negative-dt candidates whose block A (symbols 0-6, the first ~0.16s)
    // falls outside the recorded window -- exactly the population the
    // capture-window lead also targets. Includes 0.0 as an in-window
    // sanity control (both mechanisms should be no-ops or near-no-ops
    // there).
    let dt_grid: Vec<f64> = vec![-1.6, -1.2, -1.0, -0.8, -0.6, -0.3, 0.0];
    let leads = [("default", DEFAULT_LEAD), ("extended", EXTENDED_LEAD)];
    let partials = [("partial_off", false), ("partial_on", true)];

    println!(
        "Task W5.4 costas_partial_metric_enabled x W5.3 capture-window pairing — SNR={SNR_DB}dB, N={REPS_PER_DT}/cell\n"
    );
    println!(
        "{:>6} | {:>18} | {:>18} | {:>18} | {:>18}",
        "dt",
        "default/partial_off",
        "default/partial_on",
        "extended/partial_off",
        "extended/partial_on"
    );

    let mut totals: std::collections::BTreeMap<(&str, &str), (u32, u32, std::time::Duration)> =
        std::collections::BTreeMap::new();

    for &dt in &dt_grid {
        let mut row = Vec::new();
        for (lead_name, lead) in &leads {
            for (partial_name, partial) in &partials {
                let mut ok = 0u32;
                let mut elapsed_total = std::time::Duration::ZERO;
                for rep in 0..REPS_PER_DT {
                    let seed = (dt * 1000.0) as u64 ^ (rep as u64).wrapping_mul(0x9E3779B97F4A7C15);
                    let (hit, el) = trial(dt, *lead, *partial, seed);
                    if hit {
                        ok += 1;
                    }
                    elapsed_total += el;
                }
                let e = totals.entry((lead_name, partial_name)).or_insert((
                    0,
                    0,
                    std::time::Duration::ZERO,
                ));
                e.0 += ok;
                e.1 += REPS_PER_DT as u32;
                e.2 += elapsed_total;
                row.push(format!(
                    "{ok}/{REPS_PER_DT} ({:.0}%)",
                    100.0 * ok as f64 / REPS_PER_DT as f64
                ));
            }
        }
        println!(
            "{:>6.2} | {:>18} | {:>18} | {:>18} | {:>18}",
            dt, row[0], row[1], row[2], row[3]
        );
    }

    println!("\n=== Overall recall + elapsed cost per configuration ===");
    for ((lead_name, partial_name), (ok, n, elapsed)) in &totals {
        let rec = 100.0 * *ok as f64 / *n as f64;
        let avg_ms = elapsed.as_secs_f64() * 1000.0 / *n as f64;
        println!(
            "  lead={lead_name:<9} partial={partial_name:<11} recall={ok}/{n} ({rec:.1}%) avg_decode_ms={avg_ms:.1}"
        );
    }

    println!("\n=== FP-on-noise check (N=200/cell) ===");
    const NOISE_REPS: usize = 200;
    for (lead_name, lead) in &leads {
        for (partial_name, partial) in &partials {
            let mut fp = 0u32;
            for rep in 0..NOISE_REPS {
                let seed = 0xABCD_0000_u64 ^ (rep as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
                if noise_trial(*lead, *partial, seed) {
                    fp += 1;
                }
            }
            println!("  lead={lead_name:<9} partial={partial_name:<11} false_positives={fp}/{NOISE_REPS}");
        }
    }
}
