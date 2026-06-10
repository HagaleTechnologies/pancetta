//! Batch 55 — Synthesize a sparse-signal corpus and re-measure
//! auto_passband.
//!
//! Batch 53 measured `auto_passband_enabled = true` on hard-200 and saw
//! -1693 TPs (-31.9% recall). The hypothesis was that hard-200's dense
//! multi-signal slots violate the mechanism's "noise-floor-dominated
//! spectrum" assumption — the 95th-percentile-of-smoothed-spectrum peak
//! estimator picks up real signals as "noise floor," narrowing the
//! detected passband to exclude bins containing other real signals.
//!
//! This probe builds a **sparse-signal** synthetic corpus (1-3 FT8
//! signals per 15s slot at varying SNRs) and re-measures. Hypothesis:
//! on a sparse band, the 95th percentile actually IS the noise floor,
//! and auto_passband becomes net-positive.
//!
//! Corpus characteristics:
//!   - 50 sparse slots
//!   - 1-3 signals per slot (seeded RNG: round-robin 1, 2, 3, 1, 2, 3…)
//!   - Random freq offsets in [500, 2800] Hz (FT8 audio band)
//!   - SNRs in [-22, -10] dB in 2500 Hz reference bandwidth
//!   - Distinct callsigns per signal so each is a unique TP target
//!
//! Two configs at `max_decode_passes = 2, ldpc_iterations = 200`:
//!   1. baseline (auto_passband OFF)
//!   2. auto_passband ON
//!
//! Per slot, count: TPs (decoded targets matching known plant), FPs
//! (decoded text NOT in the plant set), recall = TPs / plant_count.
//!
//! Decision rule:
//!   - If auto_passband ON ≥ baseline recall AND FPs ≤ baseline → ship
//!     as tier-gated default-ON for sparse operating modes (or low-tier
//!     gate it under autonomous-station "quiet band" detection)
//!   - If auto_passband ON < baseline recall → still net-negative; the
//!     dense-band hypothesis was wrong, the mechanism just doesn't work
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch55_sparse_signal_auto_passband

use anyhow::Result;
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::HashSet;

const SAMPLE_RATE: f32 = 12_000.0;

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
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32
}

fn sigma_for_snr_db(p_signal: f32, snr_db: f32) -> f32 {
    let bw: f32 = 2500.0;
    let p_n_in_bw = p_signal / 10.0f32.powf(snr_db / 10.0);
    let nyquist = SAMPLE_RATE / 2.0;
    let sigma2 = p_n_in_bw * nyquist / bw;
    sigma2.sqrt()
}

/// One synthetic slot with a known plant set.
struct SparseSlot {
    samples: Vec<f32>,
    plant: HashSet<String>,
}

fn synth_sparse_corpus(n_slots: usize) -> Result<Vec<SparseSlot>> {
    let mut encoder = Ft8Encoder::new();
    let mut modulator = Ft8Modulator::new_default()?;

    // A pool of distinct FT8-valid messages. We use real callsign-grid
    // pairs so the message parser is happy.
    let messages = [
        "CQ K5ARH EM10",
        "CQ KC1WIH FN42",
        "CQ W1AW FN31",
        "CQ N0AX DM79",
        "CQ K1JT FN20",
        "CQ AA7BQ DM43",
        "CQ K6CW CM87",
        "CQ N5JR EM12",
        "CQ AC0JG EN90",
        "K5ARH KC1WIH FN42",
        "KC1WIH K5ARH EM10",
        "W1AW N0AX DM79",
        "K1JT AA7BQ DM43",
        "N5JR K6CW CM87",
    ];

    let mut slots = Vec::with_capacity(n_slots);
    for slot_idx in 0..n_slots {
        let mut rng = StdRng::seed_from_u64(20260609 + slot_idx as u64);

        // Round-robin 1, 2, 3, 1, 2, 3...
        let n_signals = (slot_idx % 3) + 1;

        let mut combined = vec![0.0f32; WINDOW_SAMPLES];
        let mut plant: HashSet<String> = HashSet::new();
        let mut total_signal_power = 0.0f32;

        for sig_i in 0..n_signals {
            // Pick a message (rotate through pool, salted by slot)
            let msg_idx = (slot_idx * 7 + sig_i * 13) % messages.len();
            let msg = messages[msg_idx];

            // Random audio freq in [500, 2400] Hz. The modulator's
            // hard limit is total_freq + 7 * tone_spacing <= 2500, i.e.
            // audio_freq <= 2456.25; we use 2400 as a safe upper bound.
            let audio_freq: f32 = rng.gen_range(500.0..2400.0);
            let offset_from_base = audio_freq - 1500.0;

            let symbols = encoder
                .encode_message(msg, None)
                .map_err(|e| anyhow::anyhow!("encode {msg}: {e}"))?;
            let mut tx = modulator
                .modulate_symbols(&symbols, offset_from_base as f64)
                .map_err(|e| anyhow::anyhow!("modulate {msg}: {e}"))?;
            tx.resize(WINDOW_SAMPLES, 0.0);

            let p_sig = signal_power(&tx);
            total_signal_power += p_sig;

            // Random per-signal scaling so they don't all have the same
            // amplitude (mimics real-band capture-effect variation).
            let amp_db: f32 = rng.gen_range(-3.0..3.0);
            let amp_scale = 10.0f32.powf(amp_db / 20.0);

            for (out, sample) in combined.iter_mut().zip(&tx) {
                *out += sample * amp_scale;
            }

            plant.insert(msg.to_string());
        }

        // Add noise at SNR drawn from [-22, -10] dB relative to the
        // average signal power across this slot's plants.
        let avg_signal_power = total_signal_power / n_signals as f32;
        let snr_db: f32 = rng.gen_range(-22.0..-10.0);
        let sigma = sigma_for_snr_db(avg_signal_power, snr_db);
        let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
        for (out, n) in combined.iter_mut().zip(&noise) {
            *out += n;
        }

        slots.push(SparseSlot {
            samples: combined,
            plant,
        });
    }
    Ok(slots)
}

#[derive(Default, Debug)]
struct RunStats {
    total_decodes: usize,
    total_tps: usize,
    total_fps: usize,
    total_plants: usize,
    elapsed_secs: f64,
}

fn run(slots: &[SparseSlot], cfg: &Ft8Config) -> Result<RunStats> {
    let mut stats = RunStats::default();
    let t0 = std::time::Instant::now();
    for slot in slots {
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&slot.samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        stats.total_decodes += decoded.len();
        stats.total_plants += slot.plant.len();
        for d in &decoded {
            if slot.plant.contains(&d.text) {
                stats.total_tps += 1;
            } else {
                stats.total_fps += 1;
            }
        }
    }
    stats.elapsed_secs = t0.elapsed().as_secs_f64();
    Ok(stats)
}

fn main() -> Result<()> {
    let n_slots: usize = std::env::var("BATCH55_N_SLOTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    println!("## Batch 55 — sparse-signal auto_passband re-measure");
    println!("  synthesizing {} sparse slots…", n_slots);
    let slots = synth_sparse_corpus(n_slots)?;
    let total_plants: usize = slots.iter().map(|s| s.plant.len()).sum();
    println!("  total plants across corpus: {}", total_plants);

    let mk = |ap: bool| Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        auto_passband_enabled: ap,
        ..Ft8Config::default()
    };

    eprintln!("baseline (auto_passband OFF)…");
    let s_off = run(&slots, &mk(false))?;
    let recall_off = s_off.total_tps as f64 / s_off.total_plants.max(1) as f64;
    println!(
        "\nbaseline (OFF): {} decodes / {} TPs / {} FPs / recall {:.4} ({:.1}s)",
        s_off.total_decodes, s_off.total_tps, s_off.total_fps, recall_off, s_off.elapsed_secs
    );

    eprintln!("auto_passband ON…");
    let s_on = run(&slots, &mk(true))?;
    let recall_on = s_on.total_tps as f64 / s_on.total_plants.max(1) as f64;
    let delta_tps = s_on.total_tps as i64 - s_off.total_tps as i64;
    let delta_fps = s_on.total_fps as i64 - s_off.total_fps as i64;
    println!(
        "auto_passband ON: {} decodes / {} TPs / {} FPs / recall {:.4} ({:.1}s) Δtp={:+} Δfp={:+}",
        s_on.total_decodes,
        s_on.total_tps,
        s_on.total_fps,
        recall_on,
        s_on.elapsed_secs,
        delta_tps,
        delta_fps
    );

    let decision = if delta_tps >= 0 && delta_fps <= 0 {
        "**auto_passband is net-positive on sparse**: consider tier-gating default-ON for low-occupancy operating modes."
    } else if delta_tps > 0 && delta_fps > 0 {
        format!(
            "**Mixed**: TPs lift but FPs also lift. Per-FP cost = {:.2} TPs. Inspect trade-off before any tier-gating decision.",
            delta_tps as f64 / delta_fps as f64
        )
        .leak()
    } else if delta_tps < 0 {
        "**Still net-negative on sparse**: the dense-band hypothesis was incorrect; auto_passband loses TPs even on sparse corpora. Stays default-OFF; close the line."
    } else {
        "**Net-zero on sparse**: mechanism is inert here. Stays default-OFF."
    };

    let ws = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let notes_path = ws.join("research/notes/2026-06-09-batch55-sparse-auto-passband.md");
    let body = format!(
        "# Batch 55 — sparse-signal auto_passband re-measurement\n\n\
         Synthetic sparse corpus: {} slots, 1-3 signals/slot at SNRs in \
         [-22, -10] dB (2500 Hz BW), random freq in [500, 2800] Hz. Plants \
         total = {}.\n\n\
         Config: `max_decode_passes = 2, ldpc_iterations = 200`. Only \
         `auto_passband_enabled` toggled.\n\n\
         | Config | Decodes | TPs | FPs | Recall | Elapsed |\n\
         |---|---:|---:|---:|---:|---:|\n\
         | baseline (OFF) | {} | {} | {} | {:.4} | {:.1}s |\n\
         | auto_passband ON | {} | {} | {} | {:.4} | {:.1}s |\n\n\
         **Δ TPs**: {:+}\n\
         **Δ FPs**: {:+}\n\n\
         ## Decision\n\n{}\n\n\
         ## Comparison to Batch 53 dense-band measurement\n\n\
         - Batch 53 (hard-200, dense): -1693 TPs (-31.9% recall)\n\
         - Batch 55 (sparse synthetic): {:+} TPs ({:+.2}% recall absolute)\n\n\
         The hypothesis was that auto_passband's failure on hard-200 was \
         driven by signal-dense slots violating the noise-floor-dominated \
         assumption. The sparse measurement {} that hypothesis.\n",
        n_slots,
        total_plants,
        s_off.total_decodes,
        s_off.total_tps,
        s_off.total_fps,
        recall_off,
        s_off.elapsed_secs,
        s_on.total_decodes,
        s_on.total_tps,
        s_on.total_fps,
        recall_on,
        s_on.elapsed_secs,
        delta_tps,
        delta_fps,
        decision,
        delta_tps,
        (recall_on - recall_off) * 100.0,
        if delta_tps >= 0 {
            "SUPPORTS"
        } else {
            "FALSIFIES"
        },
    );
    std::fs::write(&notes_path, body)?;
    println!("\nDone. Results in {}", notes_path.display());
    Ok(())
}
