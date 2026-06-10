//! Batch 63 — re-run the hb-244 soft combiner synthetic with widened
//! cache-key tolerance.
//!
//! Batch 62 found hb-244 inert on noise-jittered synthetic repeats: the
//! combiner's coarse (freq_bin, time_bin) key drifts across receptions
//! because each reception's sync candidate lands at a slightly different
//! freq_bin due to noise. Batch 63 adds a `soft_combiner_key_tolerance`
//! config knob (default 0 = byte-identical), then re-measures with
//! tolerance=1.
//!
//! Four configurations on the same Batch-62 synthetic ladder:
//!
//!   1. Standalone (combiner OFF, fresh decoder per reception)
//!   2. Persistent decoder, combiner ON, key_tolerance=0 (Batch 62 finding)
//!   3. Persistent decoder, combiner ON, key_tolerance=1
//!   4. Persistent decoder, combiner ON, key_tolerance=2
//!
//! If (3) or (4) recovers more receptions than (2), the cache-key
//! widening turns hb-244 from inert to net-positive. Graduation
//! candidate at that point.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch63_combiner_key_tolerance

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

fn main() -> Result<()> {
    // Same SNR ladder as Batch 62's preferred config — closer-spaced
    // values bracketing the recovery cliff at -19/-20 dB.
    let snrs: Vec<f32> = std::env::var("BATCH63_SNRS")
        .unwrap_or_else(|_| "-17.0,-18.0,-19.0,-20.0,-21.0,-22.0".to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let target = "CQ K5ARH EM10";

    println!("## Batch 63 — hb-244 with widened cache-key tolerance");
    println!("  message: {}", target);
    println!("  SNRs (dB, 2500 Hz BW): {:?}", snrs);

    let receptions = synth_repeats(target, &snrs)?;

    let mk = |on: bool, tol: u32| Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        soft_combiner_enabled: on,
        soft_combiner_key_tolerance: tol,
        ..Ft8Config::default()
    };

    eprintln!("(1) standalone, combiner OFF…");
    let hits_standalone = run_standalone(&receptions, &mk(false, 0), target)?;
    eprintln!("(2) combiner ON, tol=0 (Batch 62 finding)…");
    let hits_tol0 = run_persistent(&receptions, &mk(true, 0), target)?;
    eprintln!("(3) combiner ON, tol=1…");
    let hits_tol1 = run_persistent(&receptions, &mk(true, 1), target)?;
    eprintln!("(4) combiner ON, tol=2…");
    let hits_tol2 = run_persistent(&receptions, &mk(true, 2), target)?;

    println!("\n| SNR (dB) | (1) Standalone | (2) ON, tol=0 | (3) ON, tol=1 | (4) ON, tol=2 |");
    println!("|---:|:---:|:---:|:---:|:---:|");
    for (i, &snr) in snrs.iter().enumerate() {
        let m = |b: bool| if b { "✓" } else { "✗" };
        println!(
            "| {:+.1} | {} | {} | {} | {} |",
            snr,
            m(hits_standalone[i]),
            m(hits_tol0[i]),
            m(hits_tol1[i]),
            m(hits_tol2[i])
        );
    }

    let total = |v: &[bool]| v.iter().filter(|&&b| b).count();
    let t1 = total(&hits_standalone);
    let t2 = total(&hits_tol0);
    let t3 = total(&hits_tol1);
    let t4 = total(&hits_tol2);
    println!("\n**Totals**: (1)={t1}, (2)={t2}, (3)={t3}, (4)={t4}");
    println!(
        "**Gains**: tol=1 vs tol=0: {:+}, tol=2 vs tol=0: {:+}, tol=2 vs tol=1: {:+}",
        t3 as i64 - t2 as i64,
        t4 as i64 - t2 as i64,
        t4 as i64 - t3 as i64
    );

    let best_gain = (t3 as i64).max(t4 as i64) - t2 as i64;
    let decision = if best_gain >= 1 {
        format!(
            "**Widened key tolerance turns hb-244 from inert to net-positive on this synthetic** (+{best_gain} reception). Hard-corpus measurement next before considering default flip."
        )
    } else if best_gain == 0 {
        "**Widening provides no lift on this synthetic**: the issue is not cache-key drift alone. Possibilities: (a) sync stage fails entirely at the failed SNRs (no candidates to combine), (b) freq drift exceeds 2 bins, (c) the LDPC convergence requires more than 2-3x LLR accumulation.".to_string()
    } else {
        format!(
            "**Widening regresses ({best_gain})**: false positives at the wider tolerance bring in spurious matches."
        )
    };
    println!("\n## Decision\n\n{decision}\n");

    let ws = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let notes_path = ws.join("research/notes/2026-06-09-batch63-combiner-tolerance.md");
    let body = format!(
        "# Batch 63 — hb-244 with widened cache-key tolerance\n\n\
         SNR ladder: {:?} dB (2500 Hz BW). Same Batch 62 synthetic.\n\n\
         | Config | TPs recovered |\n|---|---:|\n\
         | (1) Standalone (combiner OFF) | {}/{} |\n\
         | (2) Combiner ON, tol=0 | {}/{} |\n\
         | (3) Combiner ON, tol=1 | {}/{} |\n\
         | (4) Combiner ON, tol=2 | {}/{} |\n\n\
         Gains: tol=1 vs tol=0: {:+}, tol=2 vs tol=0: {:+}, tol=2 vs tol=1: {:+}.\n\n\
         {decision}\n",
        snrs,
        t1,
        snrs.len(),
        t2,
        snrs.len(),
        t3,
        snrs.len(),
        t4,
        snrs.len(),
        t3 as i64 - t2 as i64,
        t4 as i64 - t2 as i64,
        t4 as i64 - t3 as i64,
    );
    std::fs::write(&notes_path, body)?;
    Ok(())
}
