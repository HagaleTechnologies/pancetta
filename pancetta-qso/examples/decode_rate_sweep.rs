//! Decode-rate vs SNR sweep — the decoder's sensitivity curve.
//!
//! This is a thin driver over the high-fidelity sim path
//! ([`pancetta_qso::sim::Sim::inject_signal`], feature `sim-hifi`): it encodes
//! one standard FT8 message, modulates it, adds calibrated wideband AWGN (and
//! optional fading), then decodes through the **real** pancetta-ft8 pipeline —
//! exactly the production decoder. For each requested SNR it repeats the
//! injection `N` times (a fresh noise realization per trial) and reports the
//! **decode rate** (fraction recovered). The classic FT8 sensitivity figure is
//! the ~50% decode threshold; WSJT-X lands near **-21 dB**, so a healthy
//! curve here should cross 50% somewhere in that neighbourhood.
//!
//! It also sweeps a couple of fading profiles as separate sections so the hit
//! that fading takes out of decode rate is visible side-by-side.
//!
//! Reproducibility: each trial uses `base_seed + trial_offset` as the
//! high-fidelity AWGN seed, so the whole sweep replays byte-identically for a
//! given base seed.
//!
//! Run with:
//!
//! ```bash
//! # Default: N=40, SNR -26..-12 dB (the decode cliff) — ~1800 real decodes.
//! cargo run -p pancetta-qso --features sim-hifi --example decode_rate_sweep
//! # Optional positional args: <base_seed> <N> <snr_lo> <snr_hi>.
//! # The textbook full-range picture (slower, ~3800 decodes):
//! cargo run -p pancetta-qso --features sim-hifi --example decode_rate_sweep -- 0xF18 40 -28 3
//! ```

use pancetta_qso::sim::{FadingProfile, Sim};

/// The standard FT8 message swept at every SNR. A plain CQ — representative of
/// the most common on-air frame.
const SWEEP_MESSAGE: &str = "CQ K5ARH EM10";

/// Audio offset (Hz) the signal is modulated at. Mid-band, well inside the
/// modulator's usable ~[200, 2456] Hz range.
const SWEEP_FREQ_HZ: f64 = 1200.0;

/// One row of the sweep: a requested SNR and the aggregate outcome over N trials.
struct SnrRow {
    requested_snr_db: f32,
    decoded: usize,
    n: usize,
    /// Average of the decoder's measured (WSJT-X 2500 Hz) SNR over the hits.
    measured_snr_avg: Option<f32>,
}

impl SnrRow {
    fn rate(&self) -> f32 {
        if self.n == 0 {
            0.0
        } else {
            self.decoded as f32 / self.n as f32
        }
    }
}

/// Run the full N-trial sweep across `snrs` for one fading profile.
///
/// Each trial is an independent `Sim` seeded with `base_seed + offset`, so the
/// noise realizations differ trial-to-trial yet replay deterministically.
async fn sweep_profile(
    snrs: &[f32],
    n: usize,
    base_seed: u64,
    fading: FadingProfile,
) -> Vec<SnrRow> {
    let mut rows = Vec::with_capacity(snrs.len());
    for (snr_idx, &snr) in snrs.iter().enumerate() {
        let mut decoded = 0usize;
        let mut measured_sum = 0.0f32;
        for trial in 0..n {
            // A distinct, reproducible seed per (snr, trial). Spread the SNR
            // index well apart so adjacent SNRs never share a seed.
            let seed = base_seed
                .wrapping_add((snr_idx as u64) * 100_003)
                .wrapping_add(trial as u64);
            let mut sim = Sim::new("K5ARH", Some("EM10")).await.with_hifi_seed(seed);
            let out = sim.inject_signal(SWEEP_MESSAGE, SWEEP_FREQ_HZ, snr, fading);
            if out.decoded {
                decoded += 1;
                measured_sum += out.measured_snr_db.unwrap_or(0.0);
            }
        }
        let measured_snr_avg = if decoded > 0 {
            Some(measured_sum / decoded as f32)
        } else {
            None
        };
        rows.push(SnrRow {
            requested_snr_db: snr,
            decoded,
            n,
            measured_snr_avg,
        });
    }
    rows
}

/// Linearly interpolate the requested SNR at which the decode rate crosses 50%.
///
/// Walks the (monotone-ish) curve from low to high SNR and returns the first
/// crossing of 0.5. Returns `None` if the curve never reaches 50% (or starts
/// already above it).
fn threshold_50pct(rows: &[SnrRow]) -> Option<f32> {
    for w in rows.windows(2) {
        let (lo, hi) = (&w[0], &w[1]);
        let (r0, r1) = (lo.rate(), hi.rate());
        if r0 < 0.5 && r1 >= 0.5 {
            // Interpolate between the two SNRs at rate == 0.5.
            let span = r1 - r0;
            let frac = if span.abs() < f32::EPSILON {
                0.0
            } else {
                (0.5 - r0) / span
            };
            return Some(lo.requested_snr_db + frac * (hi.requested_snr_db - lo.requested_snr_db));
        }
    }
    None
}

/// Render one profile's sweep as a table plus an ASCII rate curve.
fn print_profile(label: &str, rows: &[SnrRow]) {
    println!("\n=== {label} ===");
    println!(" SNR(req)  decoded/N   rate   bar                      measured-SNR(avg)");
    for row in rows {
        let pct = (row.rate() * 100.0).round() as u32;
        // 20-cell bar.
        let filled = (row.rate() * 20.0).round() as usize;
        let bar: String = "#".repeat(filled) + &".".repeat(20 - filled);
        let measured = match row.measured_snr_avg {
            Some(m) => format!("{m:+.1} dB"),
            None => "   —   ".to_string(),
        };
        println!(
            " {:+4.0} dB   {:>3}/{:<3}    {:>3}%  [{}]  {}",
            row.requested_snr_db, row.decoded, row.n, pct, bar, measured
        );
    }
    match threshold_50pct(rows) {
        Some(t) => println!("  -> ~50% decode threshold (interpolated): {t:+.1} dB"),
        None => println!("  -> ~50% decode threshold: not reached within the swept range"),
    }
}

#[tokio::main]
async fn main() {
    // ---- CLI args (all optional, positional): base_seed N snr_lo snr_hi ----
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Seed accepts decimal or `0x`-prefixed hex.
    let base_seed: u64 = args
        .first()
        .and_then(|s| {
            s.strip_prefix("0x")
                .map(|h| u64::from_str_radix(h, 16))
                .unwrap_or_else(|| s.parse())
                .ok()
        })
        .unwrap_or(0xF18);
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(40);
    // Default window is centered on the FT8 decode cliff (where the rate goes
    // 0% -> 100% in ~4 dB) and the dropout cliff a few dB above it, so the full
    // sensitivity transition is captured for every profile while keeping the
    // run to ~1800 real decodes. Widen with CLI args for the textbook
    // -28..+3 dB picture (e.g. `... -- 0xF18 40 -28 3`).
    let snr_lo: i32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(-26);
    let snr_hi: i32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(-12);

    let snrs: Vec<f32> = (snr_lo..=snr_hi).map(|s| s as f32).collect();

    println!("\n############################################################");
    println!("# Decode-rate vs SNR sweep — real encode/modulate/noise/decode");
    println!("############################################################");
    println!(
        "  message : {SWEEP_MESSAGE:?} @ {SWEEP_FREQ_HZ:.0} Hz\n  trials/N: {n}   SNR range: {snr_lo}..={snr_hi} dB (1 dB steps)\n  base seed: {base_seed:#x}   (deterministic; replay-stable)"
    );
    println!(
        "  SNR convention: REQUESTED is wideband-RMS (research gen_synth);\n  measured-SNR is the decoder's own WSJT-X 2500 Hz figure."
    );

    // No fading — the reference sensitivity curve.
    let clean = sweep_profile(&snrs, n, base_seed, FadingProfile::None).await;
    print_profile("No fading (reference sensitivity curve)", &clean);

    // Flat attenuation — a deterministic 3 dB loss stacked on top of the SNR
    // request. (Positive `attenuation_db` = quieter; the AWGN is calibrated to
    // the post-fading RMS, so this mostly shifts where calibration lands.)
    let flat = sweep_profile(
        &snrs,
        n,
        base_seed,
        FadingProfile::Flat {
            attenuation_db: 3.0,
        },
    )
    .await;
    print_profile("Flat fade (-3 dB attenuation across the frame)", &flat);

    // Dropout — the signal vanishes for the trailing 30% of the frame. The
    // decoder loses energy/sync and the rate curve shifts right.
    let dropout = sweep_profile(
        &snrs,
        n,
        base_seed,
        FadingProfile::Dropout { fraction: 0.3 },
    )
    .await;
    print_profile(
        "Dropout fade (signal gone for trailing 30% of frame)",
        &dropout,
    );

    // ---- Summary: thresholds side by side ----
    println!("\n=== Sensitivity summary (~50% decode threshold) ===");
    let fmt = |o: Option<f32>| match o {
        Some(t) => format!("{t:+.1} dB"),
        None => "not reached".to_string(),
    };
    println!("  no fading       : {}", fmt(threshold_50pct(&clean)));
    println!("  flat -3 dB      : {}", fmt(threshold_50pct(&flat)));
    println!("  dropout 30%     : {}", fmt(threshold_50pct(&dropout)));
    println!("  (WSJT-X reference FT8 sensitivity is ~-21 dB.)\n");
}
