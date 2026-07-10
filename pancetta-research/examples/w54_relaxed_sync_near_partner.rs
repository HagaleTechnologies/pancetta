//! Task W5.4 (decoder-tp-sensitivity plan) [A/B]: retest
//! `Ft8Config::relaxed_sync_near_partner_hz_radius` /
//! `_score_delta` and choose a real, non-zero delta.
//!
//! The mechanism (`Ft8Decoder::costas_sync_search_with_threshold_and_partner`,
//! `pancetta-ft8/src/decoder.rs:5514-5630`, JTDX-inspired,
//! `research/specs/spec-jtdx-relaxed-sync-near-partner.md`) is fully wired:
//! when a per-call `partner_freq_hz` is supplied AND
//! `relaxed_sync_near_partner_hz_radius = Some(r)`, any Costas sync
//! candidate within +/-r Hz of the partner gets a relaxed acceptance
//! threshold `max(0, min_sync_score + relaxed_sync_near_partner_score_delta)`.
//! The shipped default is radius=None, delta=0.0 -- structurally wired but
//! inert until an operator (or, per this task, a real measurement) sets
//! both to real values. Per the field's own doc comment, JTDX's reference
//! constant (1.1 on a percentile-normalized linear scale) does NOT
//! numerically transfer to pancetta's raw dB-difference `min_sync_score`
//! scale (threshold 3.0), so this harness performs the empirical
//! recalibration the doc calls for: embed a synthetic partner reply
//! ("W1ABC K5ARH RR73", matching the RR73/73-at-low-SNR scenario the
//! mechanism targets) at a known partner frequency, sweep SNR (marginal
//! range where recall is neither ~0% nor ~100%) and delta, and measure
//! both recall lift and FP-on-noise cost at the JTDX reference radius
//! (3.0 Hz).
//!
//! Because the standard `eval`/`compare` hard-200/noise_1000 corpora have
//! no "QSO partner" concept (no per-WAV partner-frequency truth), this
//! harness's SNR-sweep recall numbers are the primary/decision-driving
//! measurement for this mechanism (mirroring how W5.3 had to build its own
//! corpus for its similarly un-exercisable-by-hard-200 flag). A secondary,
//! corpus-level FP sanity check (forcing a fixed `--partner-freq-hz` across
//! hard-200 + noise_1000 via the new `eval` CLI flags) is run separately
//! and logged alongside this.
//!
//! Usage: `cargo run --release -p pancetta-research --example
//! w54_relaxed_sync_near_partner`

use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator};
use pancetta_research::synth::{add_awgn_2500hz_ref, signal_rms};
use std::time::Instant;

const SR: usize = 12_000;
const PARTNER_FREQ_HZ: f64 = 1500.0;
const RADIUS_HZ: f64 = 3.0; // JTDX reference constant, retained.
const REPS_PER_POINT: usize = 30;
const NOISE_REPS: usize = 150;

/// Rolling-capture-buffer + boundary-anchored slice, mirroring
/// `coordinator/dsp.rs::boundary_anchored_slice` (verified digit-for-digit
/// against production in Task W5.3) -- the SAME construction W5.3's own
/// synthetic harness used and validated (50-75% recall at -16dB for a
/// well-aligned message), so this reuses a proven-working noise/placement
/// convention rather than inventing a new one.
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

const MSG_SAMPLES: usize = (SR as f64 * 12.64) as usize;
const LEAD_SECS: f64 = 0.5;

/// One trial: a partner "RR73" reply exactly at `PARTNER_FREQ_HZ`,
/// well-aligned in time (dt=0 -- this mechanism targets SIGNAL margin, not
/// timing misalignment, unlike the W5.3/W5.4-capture-window pairing).
fn trial(snr_db: f64, radius: Option<f64>, delta: f64, seed: u64) -> (bool, std::time::Duration) {
    let mut enc = Ft8Encoder::new();
    let symbols = enc.encode_message("W1ABC K5ARH RR73", None).unwrap();
    let mut modu = Ft8Modulator::new_default().unwrap();
    let mut msg_audio = modu.modulate_symbols(&symbols, PARTNER_FREQ_HZ).unwrap();
    let clean_rms = signal_rms(&msg_audio);
    add_awgn_2500hz_ref(&mut msg_audio, clean_rms, snr_db, seed);

    let boundary_secs = 10.0_f64;
    let emit_now_secs = boundary_secs + 13.0;
    let buffer_len = (emit_now_secs * SR as f64).round() as usize;
    let mut buffer = vec![0.0f32; buffer_len];
    add_awgn_2500hz_ref(&mut buffer, clean_rms, snr_db, seed.wrapping_add(999_983));
    let msg_start = (boundary_secs * SR as f64).round() as isize; // dt=0
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
        LEAD_SECS,
    );
    let window = &buffer[start..start + len];

    let config = Ft8Config {
        relaxed_sync_near_partner_hz_radius: radius,
        relaxed_sync_near_partner_score_delta: delta,
        ..Ft8Config::default()
    };
    let mut dec = Ft8Decoder::new(config).unwrap();
    let t0 = Instant::now();
    let msgs = dec
        .decode_window_with_ap_scoped_partner(
            window,
            &Default::default(),
            None,
            radius.map(|_| PARTNER_FREQ_HZ),
        )
        .unwrap_or_default();
    let elapsed = t0.elapsed();
    (msgs.iter().any(|m| m.text.contains("K5ARH")), elapsed)
}

/// Pure-noise trial with the partner freq forced, at the given
/// radius/delta -- measures the mechanism's own FP-admission risk right
/// at the relaxed window (worst-case: a fixed known frequency is
/// relaxed on EVERY window, exactly matching real usage during an active
/// QSO).
fn noise_trial(radius: Option<f64>, delta: f64, seed: u64) -> bool {
    let boundary_secs = 10.0_f64;
    let emit_now_secs = boundary_secs + 13.0;
    let buffer_len = (emit_now_secs * SR as f64).round() as usize;
    let mut buffer = vec![0.0f32; buffer_len];
    let mut enc = Ft8Encoder::new();
    let symbols = enc.encode_message("W1ABC K5ARH RR73", None).unwrap();
    let mut modu = Ft8Modulator::new_default().unwrap();
    let ref_audio = modu.modulate_symbols(&symbols, PARTNER_FREQ_HZ).unwrap();
    let clean_rms = signal_rms(&ref_audio);
    add_awgn_2500hz_ref(&mut buffer, clean_rms, -16.0, seed);

    let (start, len) = boundary_anchored_slice(
        buffer.len(),
        emit_now_secs,
        boundary_secs,
        SR,
        MSG_SAMPLES,
        LEAD_SECS,
    );
    let window = &buffer[start..start + len];

    let config = Ft8Config {
        relaxed_sync_near_partner_hz_radius: radius,
        relaxed_sync_near_partner_score_delta: delta,
        ..Ft8Config::default()
    };
    let mut dec = Ft8Decoder::new(config).unwrap();
    let msgs = dec
        .decode_window_with_ap_scoped_partner(
            window,
            &Default::default(),
            None,
            radius.map(|_| PARTNER_FREQ_HZ),
        )
        .unwrap_or_default();
    !msgs.is_empty()
}

fn main() {
    let snr_grid: Vec<f64> = vec![-16.2, -16.4, -16.6, -16.8, -17.0];
    let delta_grid: Vec<f64> = vec![0.0, -1.0, -2.0, -3.0];

    println!(
        "Task W5.4 relaxed_sync_near_partner recalibration -- partner_freq={PARTNER_FREQ_HZ}Hz, radius={RADIUS_HZ}Hz, N={REPS_PER_POINT}/point\n"
    );

    // Baseline: mechanism off entirely (radius=None), across the SNR grid.
    println!("=== Baseline (radius=None, mechanism off) ===");
    let mut baseline_rec: Vec<(f64, u32)> = Vec::new();
    for &snr in &snr_grid {
        let mut ok = 0u32;
        for rep in 0..REPS_PER_POINT {
            let seed = (snr * 1000.0) as u64 ^ (rep as u64).wrapping_mul(0x9E3779B97F4A7C15);
            let (hit, _el) = trial(snr, None, 0.0, seed);
            if hit {
                ok += 1;
            }
        }
        println!(
            "  snr={snr:>6.1}dB  recall={ok}/{REPS_PER_POINT} ({:.1}%)",
            100.0 * ok as f64 / REPS_PER_POINT as f64
        );
        baseline_rec.push((snr, ok));
    }

    println!("\n=== Delta sweep (radius={RADIUS_HZ}Hz) ===");
    println!(
        "{:>8} | {}",
        "delta",
        snr_grid
            .iter()
            .map(|s| format!("{s:.0}dB"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    for &delta in &delta_grid {
        let mut cells = Vec::new();
        for &snr in &snr_grid {
            let mut ok = 0u32;
            for rep in 0..REPS_PER_POINT {
                let seed = (snr * 1000.0) as u64 ^ (rep as u64).wrapping_mul(0x9E3779B97F4A7C15);
                let (hit, _el) = trial(snr, Some(RADIUS_HZ), delta, seed);
                if hit {
                    ok += 1;
                }
            }
            cells.push(format!("{ok}/{REPS_PER_POINT}"));
        }
        println!("{:>8.1} | {}", delta, cells.join(" | "));
    }

    println!("\n=== FP-on-noise check (N={NOISE_REPS}/delta, radius={RADIUS_HZ}Hz forced every window) ===");
    // delta=0.0 with radius=None is the true production baseline (mechanism
    // inert); include it as the control row.
    println!("  radius=None (control, mechanism inert):");
    {
        let mut fp = 0u32;
        for rep in 0..NOISE_REPS {
            let seed = 0xF00D_0000_u64 ^ (rep as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
            if noise_trial(None, 0.0, seed) {
                fp += 1;
            }
        }
        println!("    false_positives={fp}/{NOISE_REPS}");
    }
    for &delta in &delta_grid {
        let mut fp = 0u32;
        for rep in 0..NOISE_REPS {
            let seed = 0xF00D_0000_u64 ^ (rep as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
            if noise_trial(Some(RADIUS_HZ), delta, seed) {
                fp += 1;
            }
        }
        println!("  delta={delta:>5.1}  false_positives={fp}/{NOISE_REPS}");
    }
}
