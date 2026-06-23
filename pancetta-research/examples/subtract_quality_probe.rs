//! subtract_quality_probe — hb-030 diagnostic
//!
//! Question: when pancetta-ft8's multi-pass decode "subtracts" a strong
//! signal and runs sync + LDPC again on the residual, does that pass
//! recover signals that pass 1 missed? Or does the subtraction leave
//! artifacts that mask the weaker signal?
//!
//! Method: generate WAVs containing two co-located FT8 signals at
//! controlled frequencies and SNRs:
//!   - "STRONG" signal at base_freq, SNR_strong dB
//!   - "WEAK" signal at base_freq + freq_offset_hz, SNR_weak dB
//! plus AWGN. Decode the combined WAV with max_passes ∈ {1, 2} and
//! a weak-only control. Map (strong_snr, weak_snr, freq_offset) →
//! (pass1 finds strong?, pass2 finds weak?, weak_alone decodes?).
//!
//! Three diagnostic outcomes per case:
//!   (a) pass2_finds_weak ∧ ¬pass1_finds_weak → subtraction helps
//!   (b) ¬pass2_finds_weak ∧ weak_alone_decodes → subtraction is masking
//!       the weak signal (residual artifacts hide what's recoverable)
//!   (c) ¬pass2_finds_weak ∧ ¬weak_alone_decodes → weak is below the
//!       decoder's sensitivity floor at this SNR; not a subtraction issue
//!
//! Run:
//!   cargo run --release -p pancetta-research --example subtract_quality_probe

use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator};
use rand::SeedableRng;
use rand_distr::{Distribution, Normal};

const SAMPLE_RATE: u32 = 12_000;
const WINDOW_SAMPLES: usize = 151_680; // ~12.64 s at 12 kHz

/// Encode + modulate `text` at `freq_hz` (audio Hz), returning unit-RMS-normalized samples.
fn modulate_unit_rms(text: &str, freq_hz: f64) -> anyhow::Result<Vec<f32>> {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder
        .encode_message(text, None)
        .map_err(|e| anyhow::anyhow!("Ft8Encoder failed: {e}"))?;

    let mut modulator = Ft8Modulator::new(SAMPLE_RATE, freq_hz, 1.0)
        .map_err(|e| anyhow::anyhow!("Ft8Modulator failed: {e}"))?;
    let mut samples = modulator
        .modulate_symbols(&symbols, 0.0)
        .map_err(|e| anyhow::anyhow!("modulate_symbols failed: {e}"))?;

    // Pad / truncate to the standard decode window.
    samples.resize(WINDOW_SAMPLES, 0.0);

    // Normalize to unit RMS so the caller's amplitude scaling matches a known SNR convention.
    let rms =
        (samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / samples.len() as f64).sqrt();
    if rms > 0.0 {
        let inv = (1.0 / rms) as f32;
        for s in samples.iter_mut() {
            *s *= inv;
        }
    }
    Ok(samples)
}

/// Build a two-signal WAV: `strong_amp * strong + weak_amp * weak + AWGN(stddev=noise_sigma)`.
fn build_two_signal_wav(
    strong_amp: f32,
    weak_amp: f32,
    noise_sigma: f32,
    strong_text: &str,
    strong_freq: f64,
    weak_text: &str,
    weak_freq: f64,
    seed: u64,
) -> anyhow::Result<Vec<f32>> {
    let strong = modulate_unit_rms(strong_text, strong_freq)?;
    let weak = modulate_unit_rms(weak_text, weak_freq)?;
    let mut out = vec![0.0f32; WINDOW_SAMPLES];
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let noise = Normal::new(0.0_f64, noise_sigma as f64).expect("noise stddev finite");
    for i in 0..WINDOW_SAMPLES {
        let n = noise.sample(&mut rng) as f32;
        out[i] = strong_amp * strong[i] + weak_amp * weak[i] + n;
    }
    Ok(out)
}

fn decode_with_passes(samples: &[f32], max_passes: usize) -> anyhow::Result<Vec<String>> {
    let mut cfg = Ft8Config::default();
    cfg.max_decode_passes = max_passes;
    let mut decoder = Ft8Decoder::new(cfg).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let messages = decoder
        .decode_window(samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(messages.into_iter().map(|m| m.text).collect())
}

fn contains(decodes: &[String], text: &str) -> bool {
    decodes.iter().any(|d| d.contains(text))
}

fn main() -> anyhow::Result<()> {
    // SNR convention: for a unit-RMS signal scaled by `amp`, with noise
    // stddev `sigma`, SNR = 20 * log10(amp / sigma). Pick sigma=1 and
    // derive `amp` per target SNR.
    let noise_sigma: f32 = 1.0;
    let amp_from_snr_db = |snr_db: f64| -> f32 { (10f64.powf(snr_db / 20.0)) as f32 };

    // Texts: keep distinct so the eval can tell which one decoded.
    let strong_text = "CQ K1ABC FN42";
    let weak_text = "K1ABC W9XYZ EM48";

    // Sweep grid.
    let strong_snr_db = -5.0_f64; // strong signal: clearly above threshold
    let weak_snrs_db = [-15.0, -18.0, -20.0, -22.0_f64];
    let freq_offsets_hz = [12.5, 25.0, 50.0, 100.0_f64]; // 1 bin, 2 bins, 4 bins, 8 bins
    let base_freq = 1500.0_f64;
    let seed = 42u64;

    println!("hb-030 — subtract_with_sidelobes diagnostic probe");
    println!("strong: {strong_text:?}  @ {base_freq} Hz, SNR={strong_snr_db} dB");
    println!("weak:   {weak_text:?}  @ base+Δ Hz, SNR varies\n");
    println!(
        "{:>5} {:>9} {:>5} {:>5} {:>5} {:>5} {:>7}",
        "wkSNR", "Δfreq_Hz", "p1_S", "p1_W", "p2_W", "ctlW", "verdict"
    );
    println!("{}", "-".repeat(58));

    let strong_amp = amp_from_snr_db(strong_snr_db);

    let mut counters = (0usize, 0usize, 0usize, 0usize); // a, b, c, other
    for &weak_snr in &weak_snrs_db {
        let weak_amp = amp_from_snr_db(weak_snr);

        for &df in &freq_offsets_hz {
            let weak_freq = base_freq + df;

            // Combined: strong + weak + noise
            let combined = build_two_signal_wav(
                strong_amp,
                weak_amp,
                noise_sigma,
                strong_text,
                base_freq,
                weak_text,
                weak_freq,
                seed,
            )?;

            let p1 = decode_with_passes(&combined, 1)?;
            let p2 = decode_with_passes(&combined, 2)?;

            // Control: weak alone in same noise (strong_amp=0 → no strong signal).
            let weak_only = build_two_signal_wav(
                0.0,
                weak_amp,
                noise_sigma,
                strong_text,
                base_freq,
                weak_text,
                weak_freq,
                seed,
            )?;
            let ctl = decode_with_passes(&weak_only, 1)?;

            let p1_s = contains(&p1, strong_text);
            let p1_w = contains(&p1, weak_text);
            let p2_w = contains(&p2, weak_text);
            let ctl_w = contains(&ctl, weak_text);

            let verdict = if p2_w && !p1_w {
                counters.0 += 1;
                "(a) sub helps"
            } else if !p2_w && ctl_w {
                counters.1 += 1;
                "(b) sub masks"
            } else if !p2_w && !ctl_w {
                counters.2 += 1;
                "(c) below floor"
            } else {
                counters.3 += 1;
                "other"
            };

            println!(
                "{:>5.1} {:>9.1} {:>5} {:>5} {:>5} {:>5}   {}",
                weak_snr,
                df,
                if p1_s { "Y" } else { "." },
                if p1_w { "Y" } else { "." },
                if p2_w { "Y" } else { "." },
                if ctl_w { "Y" } else { "." },
                verdict,
            );
        }
    }

    let total = counters.0 + counters.1 + counters.2 + counters.3;
    println!();
    println!("Summary across {total} cases:");
    println!(
        "  (a) subtraction surfaces a previously-missed weak signal: {}",
        counters.0
    );
    println!(
        "  (b) subtraction masks a recoverable weak signal:         {}",
        counters.1
    );
    println!(
        "  (c) weak is below floor (not a subtraction issue):       {}",
        counters.2
    );
    println!(
        "  other (e.g., pass 1 already found weak):                 {}",
        counters.3
    );

    Ok(())
}
