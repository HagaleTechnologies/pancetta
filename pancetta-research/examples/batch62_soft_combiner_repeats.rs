//! Batch 62 — hb-244 soft combiner repeat-heavy synthetic probe.
//!
//! Synthesizes N receptions of the SAME target message at progressively
//! decreasing SNRs, then runs them through a single `Ft8Decoder`
//! instance with `soft_combiner_enabled = true`. The combiner should
//! accumulate LLR evidence across receptions; weak receptions that
//! fail standalone may recover when combined with earlier stronger
//! ones.
//!
//! Three configurations on the same synthetic corpus:
//!
//!   1. **Standalone (combiner OFF)**: each reception decoded with a
//!      FRESH decoder. Establishes the per-reception standalone
//!      recovery cliff.
//!   2. **Persistent decoder, combiner OFF**: one decoder instance
//!      walked across all receptions; combiner disabled. Controls for
//!      any cross-reception state in the decoder OTHER than the
//!      combiner (there shouldn't be any).
//!   3. **Persistent decoder, combiner ON**: the production hb-244
//!      configuration. Cumulative TP count is the metric.
//!
//! The decision rule:
//!   - If (3) recovers more total TPs than (1) at the same SNR ladder,
//!     the combiner is providing real lift. The deltas are precisely
//!     the receptions the combiner saved from oblivion.
//!   - If (2) == (1), the decoder-state-sharing doesn't matter on its
//!     own (sanity check; expected to hold).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch62_soft_combiner_repeats

use anyhow::Result;
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

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

/// Synthesize N receptions of the same message at N different SNRs.
/// All at the same audio frequency offset so the combiner's coarse
/// (freq_bin, time_bin) key matches across receptions.
fn synth_repeats(message: &str, snrs_db: &[f32]) -> Result<Vec<Vec<f32>>> {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder
        .encode_message(message, None)
        .map_err(|e| anyhow::anyhow!("encode: {e}"))?;
    let mut modulator = Ft8Modulator::new_default()?;
    let mut tx = modulator
        .modulate_symbols(&symbols, 0.0)
        .map_err(|e| anyhow::anyhow!("modulate: {e}"))?;
    tx.resize(WINDOW_SAMPLES, 0.0);
    let p_signal = signal_power(&tx);

    let mut receptions = Vec::with_capacity(snrs_db.len());
    for (i, &snr_db) in snrs_db.iter().enumerate() {
        let mut rng = StdRng::seed_from_u64(20260610 + i as u64);
        let sigma = sigma_for_snr_db(p_signal, snr_db);
        let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
        let recv: Vec<f32> = tx.iter().zip(&noise).map(|(s, n)| s + n).collect();
        receptions.push(recv);
    }
    Ok(receptions)
}

fn count_tps(decoded: &[pancetta_ft8::DecodedMessage], target: &str) -> usize {
    decoded.iter().filter(|d| d.text == target).count()
}

fn run_standalone(receptions: &[Vec<f32>], cfg: &Ft8Config, target: &str) -> Result<Vec<bool>> {
    let mut hits = Vec::with_capacity(receptions.len());
    for r in receptions {
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(r)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        hits.push(count_tps(&decoded, target) > 0);
    }
    Ok(hits)
}

fn run_persistent(receptions: &[Vec<f32>], cfg: &Ft8Config, target: &str) -> Result<Vec<bool>> {
    let mut hits = Vec::with_capacity(receptions.len());
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    for r in receptions {
        let decoded = decoder
            .decode_window(r)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        hits.push(count_tps(&decoded, target) > 0);
    }
    Ok(hits)
}

fn main() -> Result<()> {
    // SNR ladder bracketing the FT8 recovery cliff. Closer-spaced
    // values target the "sync detects but LDPC fails" regime where
    // the combiner's LLR accumulation has its best chance to fire.
    // Override via BATCH62_SNRS env (comma-separated dB values).
    let snrs: Vec<f32> = std::env::var("BATCH62_SNRS")
        .unwrap_or_else(|_| "-17.0,-18.0,-19.0,-20.0,-21.0,-22.0".to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let target = "CQ K5ARH EM10";

    println!("## Batch 62 — hb-244 soft combiner repeat-heavy probe");
    println!("  message: {}", target);
    println!("  SNRs (dB, 2500 Hz BW): {:?}", snrs);

    let receptions = synth_repeats(target, &snrs)?;

    let cfg_combiner_off = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        soft_combiner_enabled: false,
        ..Ft8Config::default()
    };
    let cfg_combiner_on = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        soft_combiner_enabled: true,
        ..Ft8Config::default()
    };

    eprintln!("(1) standalone, combiner OFF…");
    let hits_standalone = run_standalone(&receptions, &cfg_combiner_off, target)?;
    eprintln!("(2) persistent decoder, combiner OFF…");
    let hits_persistent_off = run_persistent(&receptions, &cfg_combiner_off, target)?;
    eprintln!("(3) persistent decoder, combiner ON (hb-244)…");
    let hits_persistent_on = run_persistent(&receptions, &cfg_combiner_on, target)?;

    println!("\n| SNR (dB) | (1) Standalone | (2) Persistent OFF | (3) Persistent ON (hb-244) |");
    println!("|---:|:---:|:---:|:---:|");
    for (i, &snr) in snrs.iter().enumerate() {
        let mk = |b: bool| if b { "✓" } else { "✗" };
        println!(
            "| {:+.1} | {} | {} | {} |",
            snr,
            mk(hits_standalone[i]),
            mk(hits_persistent_off[i]),
            mk(hits_persistent_on[i])
        );
    }

    let total_1: usize = hits_standalone.iter().filter(|&&b| b).count();
    let total_2: usize = hits_persistent_off.iter().filter(|&&b| b).count();
    let total_3: usize = hits_persistent_on.iter().filter(|&&b| b).count();
    println!(
        "\n**Totals**: (1)={}, (2)={}, (3)={}",
        total_1, total_2, total_3
    );

    let combiner_gain = total_3 as i64 - total_2 as i64;
    let persistent_only_gain = total_2 as i64 - total_1 as i64;
    println!(
        "\n**Combiner gain** (3) - (2): **{:+}** receptions recovered\n\
         **Persistent-decoder gain** (2) - (1): **{:+}** receptions (sanity check; should be 0)",
        combiner_gain, persistent_only_gain
    );

    let decision = if combiner_gain >= 1 && persistent_only_gain == 0 {
        format!(
            "**hb-244 soft combiner shows real lift**: {} extra receptions recovered by accumulating LLR evidence. Builds the case for a hard-corpus measurement before default-ON consideration.",
            combiner_gain
        )
    } else if combiner_gain == 0 {
        "**hb-244 inert on this synthetic**: combiner doesn't change the recovery profile. Either the signal repeats are too high-SNR (combiner not needed) or too low-SNR (no usable LLR evidence). Try a different SNR ladder.".to_string()
    } else if persistent_only_gain != 0 {
        format!(
            "**Sanity check failed**: persistent-decoder-without-combiner showed {} delta from standalone. Investigate decoder cross-window state before trusting combiner results.",
            persistent_only_gain
        )
    } else {
        format!(
            "**Combiner regression**: ON loses {} receptions vs OFF. Investigate.",
            -combiner_gain
        )
    };
    println!("\n## Decision\n\n{decision}\n");

    let ws = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let notes_path = ws.join("research/notes/2026-06-09-batch62-soft-combiner.md");
    let body = format!(
        "# Batch 62 — hb-244 soft combiner repeat-heavy probe\n\n\
         Message: `{}`. SNR ladder: {:?} dB (2500 Hz BW). Each reception synthesized at the same audio frequency offset.\n\n\
         | Config | TPs recovered |\n|---|---:|\n\
         | (1) Standalone (combiner OFF, fresh decoder per reception) | {}/{} |\n\
         | (2) Persistent decoder, combiner OFF | {}/{} |\n\
         | (3) Persistent decoder, combiner ON (hb-244) | {}/{} |\n\n\
         **Combiner gain** (3) - (2): {:+}\n\
         **Persistent-only gain** (2) - (1): {:+} (sanity check)\n\n\
         {decision}\n",
        target,
        snrs,
        total_1,
        snrs.len(),
        total_2,
        snrs.len(),
        total_3,
        snrs.len(),
        combiner_gain,
        persistent_only_gain,
    );
    std::fs::write(&notes_path, body)?;
    Ok(())
}
