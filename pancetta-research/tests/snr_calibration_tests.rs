//! Task W0.2 (2026-07-06) — 2500 Hz SNR calibration.
//!
//! WSJT-X / `jt9` report decode SNR relative to a fixed 2500 Hz reference
//! noise bandwidth, not pancetta-research's pre-W0.2 full-band (6 kHz
//! Nyquist) convention — a ~3.8 dB discrepancy documented in
//! `docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md`
//! section 2. This test generates a real synth WAV at a labeled SNR,
//! writes + re-reads it (a genuine round trip, not an in-memory shortcut),
//! then INDEPENDENTLY measures the achieved SNR from the assembled noisy
//! audio via a spectral method wholly separate from
//! `synth::add_awgn_2500hz_ref`'s own time-domain formula:
//!
//! - **Signal power**: a per-symbol rectangular DFT (window = exactly one
//!   FT8 symbol span, `SAMPLES_PER_SYMBOL` samples — the same rectangular,
//!   symbol-length window the project's own design spec recommends for
//!   matched per-candidate demodulation, section 3/D3) at each of the 79
//!   known tone bins (we know the encoded message, hence every symbol's
//!   transmitted tone), averaged across all 79 symbols and debiased by the
//!   known per-bin noise contribution (`N * noise_variance`, from DFT
//!   scaling of white noise) so the measurement isolates signal energy
//!   from the additive noise actually present at those exact bins.
//! - **Noise power**: measured directly (time domain) from the WAV's
//!   silent lead-in region (guaranteed signal-free), then scaled from the
//!   full [0, 6000] Hz band down to the 2500 Hz reference band by the same
//!   `2500/6000` ratio the generator's convention is built on.
//!
//! If the generator's noise scaling matches the WSJT-X convention, this
//! independently-measured ratio should land within ±0.3 dB of the file's
//! labeled SNR. This is a genuine cross-check, not a restatement of the
//! implementation: it never calls `add_awgn_2500hz_ref`'s formula, only
//! its *output* audio.

use num_complex::Complex;
use pancetta_research::synth::{
    add_awgn_2500hz_ref, encode_message_symbols, modulate_message_at, place_in_slot, signal_rms,
    SAMPLES_PER_SYMBOL,
};
use rustfft::FftPlanner;

const SAMPLE_RATE_HZ: f64 = 12_000.0;
const REFERENCE_BANDWIDTH_HZ: f64 = 2500.0;
const FULL_BAND_HZ: f64 = SAMPLE_RATE_HZ / 2.0; // 6000 Hz Nyquist
const TONE_SPACING_HZ: f64 = 6.25;
const NUM_SYMBOLS: usize = 79;
const MESSAGE: &str = "CQ K1ABC FN42";
const BASE_FREQ_HZ: f64 = 1500.0; // multiple of TONE_SPACING_HZ -> exact bin alignment
const LEAD_IN_S: f64 = 1.0;
const SLOT_LEN_SAMPLES: usize = 180_000; // 15.0 s, matches the real corpus slot length
const LABEL_SNR_DB: f64 = -15.0;
const TOLERANCE_DB: f64 = 0.3;

/// Generate one synth WAV at `snr_db`, write it to `path`, and return the
/// re-read (post-16-bit-quantization) samples plus the (known) encoded
/// symbols and the sample index the signal starts at.
fn generate_and_reread(
    path: &std::path::Path,
    snr_db: f64,
    seed: u64,
) -> (Vec<f32>, [u8; 79], usize) {
    let symbols = encode_message_symbols(MESSAGE).expect("encode");
    let base_signal = modulate_message_at(MESSAGE, BASE_FREQ_HZ).expect("modulate");
    let rms = signal_rms(&base_signal);

    let mut padded = place_in_slot(&base_signal, 0.0, LEAD_IN_S, SLOT_LEN_SAMPLES);
    add_awgn_2500hz_ref(&mut padded, rms, snr_db, seed);

    // Write as 16-bit PCM (matches the rest of the corpus) and re-read —
    // a genuine round trip, not a shortcut through the in-memory f32 buffer.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE_HZ as u32,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    {
        let mut w = hound::WavWriter::create(path, spec).expect("create wav");
        for &s in &padded {
            let clamped = s.clamp(-1.0, 1.0);
            let i = (clamped * 32767.0) as i16;
            w.write_sample(i).expect("write sample");
        }
        w.finalize().expect("finalize wav");
    }

    let mut reader = hound::WavReader::open(path).expect("reopen wav");
    let reread: Vec<f32> = reader
        .samples::<i16>()
        .map(|s| s.expect("sample") as f32 / 32768.0)
        .collect();

    let start_sample = (LEAD_IN_S * SAMPLE_RATE_HZ).round() as usize;
    (reread, symbols, start_sample)
}

/// Independently measure the achieved SNR (WSJT-X 2500 Hz convention) of a
/// generated+re-read WAV. See module doc for the methodology.
fn measure_snr_db(samples: &[f32], symbols: &[u8; NUM_SYMBOLS], signal_start: usize) -> f64 {
    // --- Noise power: time-domain RMS over the (signal-free) lead-in,
    // trimmed a bit at each end to avoid any edge/onset effects.
    let noise_region = &samples[500..signal_start.saturating_sub(500)];
    assert!(
        noise_region.len() > 1000,
        "lead-in too short to measure noise from"
    );
    let noise_rms_measured = (noise_region
        .iter()
        .map(|&s| (s as f64).powi(2))
        .sum::<f64>()
        / noise_region.len() as f64)
        .sqrt();
    let noise_variance_full_band = noise_rms_measured * noise_rms_measured;
    let noise_power_2500 = noise_variance_full_band * (REFERENCE_BANDWIDTH_HZ / FULL_BAND_HZ);

    // --- Signal power: per-symbol rectangular DFT at the known tone bin,
    // debiased by the noise contribution to that same bin, averaged over
    // all 79 symbols.
    let n = SAMPLES_PER_SYMBOL;
    let bin_hz = SAMPLE_RATE_HZ / n as f64;
    assert!(
        (bin_hz - TONE_SPACING_HZ).abs() < 1e-9,
        "FFT bin width {bin_hz} must equal tone spacing {TONE_SPACING_HZ} for exact bin alignment"
    );
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(n);

    let noise_bin_bias = 2.0 * noise_variance_full_band / n as f64;

    let mut powers = Vec::with_capacity(NUM_SYMBOLS);
    for (i, &sym) in symbols.iter().enumerate() {
        let frame_start = signal_start + i * n;
        let frame = &samples[frame_start..frame_start + n];
        let mut buf: Vec<Complex<f64>> =
            frame.iter().map(|&s| Complex::new(s as f64, 0.0)).collect();
        fft.process(&mut buf);

        let tone_freq = BASE_FREQ_HZ + sym as f64 * TONE_SPACING_HZ;
        let tone_bin = (tone_freq / bin_hz).round() as usize;
        let mag2 = buf[tone_bin].norm_sqr();
        // Convert single-sided bin magnitude-squared to mean-square power
        // (Parseval: for a pure real cosine of amplitude A over N samples,
        // |X[k]|^2 = (A*N/2)^2 at its positive-frequency bin; mean-square
        // power = A^2/2 = |X[k]|^2 * 2 / N^2).
        let power = mag2 * 2.0 / (n as f64 * n as f64);
        let debiased = (power - noise_bin_bias).max(0.0);
        powers.push(debiased);
    }
    let avg_signal_power = powers.iter().sum::<f64>() / powers.len() as f64;

    10.0 * (avg_signal_power / noise_power_2500).log10()
}

#[test]
fn synth_wav_snr_matches_2500hz_wsjtx_convention_within_tolerance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("calibration_test_-15dB.wav");

    let (samples, symbols, signal_start) = generate_and_reread(&path, LABEL_SNR_DB, 20260706);
    let measured_db = measure_snr_db(&samples, &symbols, signal_start);

    eprintln!(
        "snr_calibration: label={LABEL_SNR_DB:.1} dB, independently-measured={measured_db:.3} dB, \
         delta={:.3} dB (tolerance ±{TOLERANCE_DB} dB)",
        measured_db - LABEL_SNR_DB
    );

    assert!(
        (measured_db - LABEL_SNR_DB).abs() <= TOLERANCE_DB,
        "measured SNR {measured_db:.3} dB is more than {TOLERANCE_DB} dB from the label \
         {LABEL_SNR_DB:.1} dB (WSJT-X 2500 Hz reference-bandwidth convention)"
    );
}
