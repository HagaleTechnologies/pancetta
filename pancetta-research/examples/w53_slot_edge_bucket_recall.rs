//! Task W5.3 (decoder-tp-sensitivity plan) [A/B]: slot-edge bucket recall
//! measurement.
//!
//! `hard-200`/`chrono_replay` (the plan's standard `eval`/`compare` corpora)
//! cannot exercise this task's fix at all: `pancetta-research`'s
//! `decode_wav` calls `pancetta_ft8::Ft8Decoder` directly on pre-recorded
//! WAV files, bypassing the coordinator's DSP capture pipeline entirely
//! (`coordinator/dsp.rs`'s `boundary_anchored_slice`, where this task's
//! `[decoder].extended_capture_window_enabled` flag actually lives). Worse,
//! the corpus WAV files themselves structurally lack the negative-dt lead
//! audio the fix depends on — confirmed by direct investigation: the
//! `pancetta-ft8/tests/fixtures/wav/wsjt/210703_133430.wav` reference
//! fixture is exactly 15.0s (180,000 samples @ 12kHz), the standard
//! WSJT-X-convention per-slot capture with sample 0 at (or essentially at)
//! the slot boundary — there is no real "prior slot tail" audio in the file
//! to find a negative-dt signal in, regardless of what any decoder or
//! capture-window change does. This matches Batch 36 C1's own diagnosis
//! (`research/experiments/2026-06-06-batch-36.md`): "A signal whose actual
//! start falls before the WAV's first sample (negative-dt in jt9's truth)
//! cannot be located by the current sync. ... To recover negative-dt
//! signals, the decoder either: (a) pre-pads input with silence [...] (b)
//! extends the spectrogram generation to include a buffer of prior-slot
//! tail samples (would need coordinator-level buffering [...])" — Batch 36
//! explicitly deferred (b) as out of scope; this task builds it.
//!
//! So this example builds a purpose-built synthetic corpus that DOES embed
//! the necessary lead-in audio (a realistic "rolling capture buffer" with
//! the message placed at a range of dt values, sliced via the exact same
//! anchoring formula `coordinator/dsp.rs::boundary_anchored_slice` uses),
//! and measures recall bucketed exactly like Batch 36's C2 table
//! (`research/experiments/2026-06-06-batch-36.md`), for BOTH the default
//! 0.5s lead and the widened 1.0s lead — giving a genuine, non-vacuous
//! measurement of "the slot-edge bucket recall specifically" (this task's
//! own instruction) that the standard corpora cannot provide.
//!
//! Usage: `cargo run --release -p pancetta-research --example
//! w53_slot_edge_bucket_recall`

use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator};
use pancetta_research::synth::{add_awgn_2500hz_ref, signal_rms};
use std::time::Instant;

const SR: usize = 12_000;
const MSG_SAMPLES: usize = (SR as f64 * 12.64) as usize;
const DEFAULT_LEAD: f64 = 0.5;
const EXTENDED_LEAD: f64 = 1.0;
/// Chosen so baseline (dt=0) recall sits well below 100% but well above the
/// capture-cliff floor — makes the slot-edge effect visible against a
/// realistic backdrop instead of being swamped by pure-SNR misses or masked
/// by every trial trivially succeeding.
const SNR_DB: f64 = -16.0;
const REPS_PER_DT: usize = 12;

/// Mirrors `coordinator/dsp.rs::boundary_anchored_slice` exactly (private
/// fn there, not reachable across the crate boundary) — verified digit-for-
/// digit against the production formula during this task's investigation.
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
/// `boundary_anchored_slice` with the given lead, decode, report
/// (hit/miss, elapsed decode time).
fn trial(dt_secs: f64, lead_secs: f64, seed: u64) -> (bool, std::time::Duration) {
    let mut enc = Ft8Encoder::new();
    let symbols = enc.encode_message("CQ K5ARH EM12", None).unwrap();
    let mut modu = Ft8Modulator::new_default().unwrap();
    let mut msg_audio = modu
        .modulate_symbols(&symbols, 500.0 + (seed % 50) as f64)
        .unwrap();
    let clean_rms = signal_rms(&msg_audio);
    add_awgn_2500hz_ref(&mut msg_audio, clean_rms, SNR_DB, seed);

    // Boundary at absolute t=10s; buffer ends exactly at emit_now
    // (decode_phase=13s past boundary, FT8 default) — the real ft8_buffer's
    // invariant (newest sample == "now").
    let boundary_secs = 10.0_f64;
    let emit_now_secs = boundary_secs + 13.0;
    let buffer_len = (emit_now_secs * SR as f64).round() as usize;
    let mut buffer = vec![0.0f32; buffer_len];
    // Independent noise floor across the whole buffer (not just the signal
    // span) so the decoder sees a realistic noisy passband, not a silent
    // one outside the message — matches how add_awgn is normally applied
    // to full slot buffers elsewhere in this crate.
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

    let mut dec = Ft8Decoder::new(Ft8Config::default()).unwrap();
    let t0 = Instant::now();
    let msgs = dec.decode_window(window).unwrap_or_default();
    let elapsed = t0.elapsed();
    (msgs.iter().any(|m| m.text.contains("K5ARH")), elapsed)
}

fn bucket_label(dt: f64) -> &'static str {
    if dt < -1.5 {
        "<-1.5"
    } else if dt < -1.0 {
        "-1.5..-1.0"
    } else if dt < -0.5 {
        "-1.0..-0.5"
    } else if dt < 0.0 {
        "-0.5..0"
    } else if dt < 0.5 {
        "0..0.5"
    } else if dt < 1.0 {
        "0.5..1.0"
    } else if dt < 1.5 {
        "1.0..1.5"
    } else if dt < 2.0 {
        "1.5..2.0"
    } else {
        ">=2.0"
    }
}

fn main() {
    // dt grid spans Batch 36's C2 buckets plus this task's specific -0.8/
    // -1.0/+2.2 targets.
    let dt_grid: Vec<f64> = vec![
        -1.6, -1.2, -1.0, -0.8, -0.6, -0.3, 0.1, 0.6, 1.2, 1.6, 2.0, 2.2,
    ];

    println!(
        "Task W5.3 slot-edge bucket recall — SNR={SNR_DB}dB, N={REPS_PER_DT}/dt, default_lead={DEFAULT_LEAD}s, extended_lead={EXTENDED_LEAD}s\n"
    );
    println!(
        "{:>12} | {:>6} | {:>10} | {:>10} | {:>14} | {:>14}",
        "bucket", "dt", "default_ok", "extended_ok", "default_rec%", "extended_rec%"
    );

    use std::collections::BTreeMap;
    let mut bucket_default: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
    let mut bucket_extended: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
    let mut total_elapsed_default = std::time::Duration::ZERO;
    let mut total_elapsed_extended = std::time::Duration::ZERO;
    let mut n_default = 0u32;
    let mut n_extended = 0u32;

    for &dt in &dt_grid {
        let mut ok_default = 0u32;
        let mut ok_extended = 0u32;
        for rep in 0..REPS_PER_DT {
            let seed = (dt * 1000.0) as u64 ^ (rep as u64).wrapping_mul(0x9E3779B97F4A7C15);
            let (hit_d, el_d) = trial(dt, DEFAULT_LEAD, seed);
            if hit_d {
                ok_default += 1;
            }
            total_elapsed_default += el_d;
            n_default += 1;
            let (hit_e, el_e) = trial(dt, EXTENDED_LEAD, seed);
            if hit_e {
                ok_extended += 1;
            }
            total_elapsed_extended += el_e;
            n_extended += 1;
        }
        let bucket = bucket_label(dt);
        let e = bucket_default.entry(bucket).or_insert((0, 0));
        e.0 += ok_default;
        e.1 += REPS_PER_DT as u32;
        let e2 = bucket_extended.entry(bucket).or_insert((0, 0));
        e2.0 += ok_extended;
        e2.1 += REPS_PER_DT as u32;

        println!(
            "{:>12} | {:>6.2} | {:>10} | {:>10} | {:>13.1}% | {:>13.1}%",
            bucket,
            dt,
            format!("{ok_default}/{REPS_PER_DT}"),
            format!("{ok_extended}/{REPS_PER_DT}"),
            100.0 * ok_default as f64 / REPS_PER_DT as f64,
            100.0 * ok_extended as f64 / REPS_PER_DT as f64
        );
    }

    println!("\n=== Bucketed (Batch-36-C2-style) recall ===");
    println!(
        "{:>12} | {:>14} | {:>14} | {:>10}",
        "bucket", "default_rec%", "extended_rec%", "delta"
    );
    for (bucket, (ok_d, n_d)) in &bucket_default {
        let (ok_e, _n_e) = bucket_extended.get(bucket).copied().unwrap_or((0, 1));
        let rec_d = 100.0 * *ok_d as f64 / *n_d as f64;
        let rec_e = 100.0 * ok_e as f64 / *n_d as f64;
        println!(
            "{:>12} | {:>13.1}% | {:>13.1}% | {:>+9.1}",
            bucket,
            rec_d,
            rec_e,
            rec_e - rec_d
        );
    }

    let avg_ms_default = total_elapsed_default.as_secs_f64() * 1000.0 / n_default.max(1) as f64;
    let avg_ms_extended = total_elapsed_extended.as_secs_f64() * 1000.0 / n_extended.max(1) as f64;
    println!("\n=== Elapsed decode cost (speed-plan gate) ===");
    println!(
        "default_lead ({DEFAULT_LEAD}s, {n_default} decodes): {avg_ms_default:.1} ms/window avg"
    );
    println!(
        "extended_lead ({EXTENDED_LEAD}s, {n_extended} decodes): {avg_ms_extended:.1} ms/window avg"
    );
    println!(
        "delta: {:+.1} ms/window ({:+.1}%)",
        avg_ms_extended - avg_ms_default,
        100.0 * (avg_ms_extended - avg_ms_default) / avg_ms_default
    );
}
