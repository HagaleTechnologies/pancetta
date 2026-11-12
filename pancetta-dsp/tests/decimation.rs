//! PAN-320 acceptance tests for the 48 kHz → 12 kHz FIR decimator
//! (`pancetta_dsp::decimation`).
//!
//! AC1 (`fir31_response_and_unity_gain`,
//! `tone_amplitude_and_alias_rejection`): the shipped taps have unity DC
//! gain, symmetry, a flat 0–4 kHz passband and ≥38 dB rejection over
//! 8–24 kHz on a 10 Hz grid; steady-state 1 kHz and 9.5 kHz tones measure
//! the same properties end-to-end through `process`.
//!
//! AC2 (`chunking_matches_reference`, `reset_matches_fresh_instance`,
//! `output_phase_and_latency`): arbitrary chunk sizes equal one batch
//! (against an independent direct-convolution reference), output phase and
//! length are stable, and reset removes all history and phase.

use pancetta_dsp::decimation::{
    design_fir31_taps, DecimationFilter, DECIMATION_FACTOR, DECIMATION_INPUT_RATE_HZ,
    DECIMATION_OUTPUT_RATE_HZ, FIR_DELAY_INPUT_SAMPLES, FIR_TAPS,
};

/// DTFT magnitude of real taps at frequency `hz` (linear power summed in
/// f64), evaluated directly — independent of any FFT crate.
fn response_db(taps: &[f64], hz: f64) -> f64 {
    let mut re = 0.0f64;
    let mut im = 0.0f64;
    let w = 2.0 * std::f64::consts::PI * hz / DECIMATION_INPUT_RATE_HZ;
    for (n, &h) in taps.iter().enumerate() {
        let ph = w * n as f64;
        re += h * ph.cos();
        im -= h * ph.sin();
    }
    20.0 * (re * re + im * im).sqrt().log10()
}

#[test]
fn fir31_response_and_unity_gain() {
    let taps = design_fir31_taps();
    assert_eq!(taps.len(), FIR_TAPS, "exactly 31 taps");

    // Symmetric Type-I FIR.
    for j in 0..FIR_TAPS / 2 {
        assert!(
            (taps[j] - taps[FIR_TAPS - 1 - j]).abs() < 1e-12,
            "tap {j} not symmetric"
        );
    }

    // Unity DC gain within 1e-6.
    let dc: f64 = taps.iter().sum();
    assert!((dc - 1.0).abs() < 1e-6, "DC gain {dc} != 1");

    // Passband 0-4000 Hz within ±0.1 dB on a 10 Hz grid.
    let mut worst_pass = 0.0f64;
    for hz in (0..=4000).step_by(10) {
        let db = response_db(&taps, hz as f64);
        worst_pass = worst_pass.max(db.abs());
        assert!(
            db.abs() <= 0.1,
            "passband gain error {db:.6} dB at {hz} Hz exceeds ±0.1 dB"
        );
    }
    // Sanity: the design is not degenerate (a broken all-zero filter would
    // also pass |db| <= 0.1 nowhere but trivially fail above).
    assert!(worst_pass < 0.1);

    // Stopband 8-24 kHz at least 38 dB down on the same grid (measured
    // floor of this design: -39.4 dB).
    let mut worst_stop = f64::MIN;
    for hz in (8000..=24000).step_by(10) {
        let db = response_db(&taps, hz as f64);
        worst_stop = worst_stop.max(db);
        assert!(
            db <= -38.0,
            "stopband gain {db:.6} dB at {hz} Hz is above -38 dB"
        );
    }
    assert!(worst_stop <= -38.0 && worst_stop > -45.0);
}

/// Complex amplitude of a tone at `hz` inside `samples` (least-squares fit
/// against e^{i 2π f n / fs}), the tone amplitude the decoder would see.
fn tone_amplitude(samples: &[f32], hz: f64, fs: f64) -> f64 {
    let mut re = 0.0f64;
    let mut im = 0.0f64;
    let w = 2.0 * std::f64::consts::PI * hz / fs;
    for (n, &s) in samples.iter().enumerate() {
        let ph = w * n as f64;
        re += s as f64 * ph.cos();
        im += s as f64 * ph.sin();
    }
    2.0 * (re * re + im * im).sqrt() / samples.len() as f64
}

#[test]
fn tone_amplitude_and_alias_rejection() {
    // 100k input samples (~2.1 s) of a 0.5-amplitude tone; the last 40k
    // outputs (~3.3 s? no: 10k outputs ≈ 0.83 s) are steady state.
    let make_tone = |hz: f64| -> Vec<f32> {
        (0..100_000)
            .map(|n| {
                (0.5 * (2.0 * std::f64::consts::PI * hz * n as f64 / DECIMATION_INPUT_RATE_HZ)
                    .sin()) as f32
            })
            .collect()
    };

    // In-band 1 kHz: unity gain through the decimator.
    let mut fir = DecimationFilter::new();
    let mut out = Vec::new();
    fir.process(&make_tone(1000.0), &mut out);
    assert_eq!(out.len(), 100_000 / DECIMATION_FACTOR);
    let steady = &out[out.len() - 10_000..];
    let amp = tone_amplitude(steady, 1000.0, DECIMATION_OUTPUT_RATE_HZ);
    assert!(
        (amp - 0.5).abs() < 0.005,
        "1 kHz amplitude {amp:.5} != 0.5 (passband must be unity gain)"
    );

    // Out-of-band 9.5 kHz: folds to 2.5 kHz under naive subsampling; the
    // FIR must suppress it (measured: about -56 dB).
    let mut fir = DecimationFilter::new();
    let mut out = Vec::new();
    fir.process(&make_tone(9500.0), &mut out);
    let steady = &out[out.len() - 10_000..];
    let amp = tone_amplitude(steady, 2500.0, DECIMATION_OUTPUT_RATE_HZ);
    assert!(
        amp < 0.5 * 10f64.powf(-30.0 / 20.0),
        "9.5 kHz folded amplitude {amp:.6} is above -30 dB (design: -56 dB)"
    );
}

/// Independent direct-convolution reference: y[k] = Σ h[j]·x[4k+3−j],
/// negative indices treated as zero (f64 accumulation over the f64 taps —
/// a different code path than the streaming f32 implementation).
fn reference_outputs(taps: &[f64], input: &[f32]) -> Vec<f64> {
    let n_out = input.len() / DECIMATION_FACTOR;
    (0..n_out)
        .map(|k| {
            let mut acc = 0.0f64;
            for (j, &h) in taps.iter().enumerate() {
                let idx = DECIMATION_FACTOR as isize * k as isize + 3 - j as isize;
                if idx >= 0 {
                    acc += h * input[idx as usize] as f64;
                }
            }
            acc
        })
        .collect()
}

/// Deterministic LCG noise in (-1, 1) — no external rand dependency.
struct Lcg(u64);
impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f32 / (u32::MAX >> 1) as f32) - 1.0
    }
}

#[test]
fn chunking_matches_reference() {
    let mut lcg = Lcg(0x5041_4E33_3230);
    let input: Vec<f32> = (0..10_007).map(|_| lcg.next_f32()).collect();
    let taps = design_fir31_taps();
    let reference = reference_outputs(&taps, &input);

    // One batch.
    let mut fir = DecimationFilter::new();
    let mut one_batch = Vec::new();
    fir.process(&input, &mut one_batch);
    assert_eq!(one_batch.len(), reference.len());
    for (k, (a, b)) in one_batch.iter().zip(reference.iter()).enumerate() {
        assert!(
            (a - *b as f32).abs() < 1e-6,
            "one-batch output {k}: {a} vs reference {b}"
        );
    }

    // Arbitrary chunking: empty, one-sample, uneven, large — appended
    // results must equal the one-batch stream exactly (phase stable), and
    // stay within 1e-6 of the reference.
    let mut fir = DecimationFilter::new();
    let mut chunked = Vec::new();
    let mut chunk_scratch: Vec<f32> = Vec::new();
    let mut pos = 0usize;
    for &size in &[0usize, 1, 1, 3, 7, 1, 511, 2, 1000, 4, 4, 4, 7, 8464] {
        let end = (pos + size).min(input.len());
        chunk_scratch.clear();
        chunk_scratch.extend_from_slice(&input[pos..end]);
        fir.process(&chunk_scratch, &mut chunked);
        pos = end;
    }
    // Remaining samples (if any) in one final ragged chunk.
    if pos < input.len() {
        chunk_scratch.clear();
        chunk_scratch.extend_from_slice(&input[pos..]);
        fir.process(&chunk_scratch, &mut chunked);
    }
    assert_eq!(chunked.len(), one_batch.len());
    for (k, (a, b)) in chunked.iter().zip(one_batch.iter()).enumerate() {
        assert!(
            (a - b).abs() < 1e-9,
            "chunked output {k} drifted from one-batch"
        );
    }
    for (k, (a, b)) in chunked.iter().zip(reference.iter()).enumerate() {
        assert!(
            (a - *b as f32).abs() < 1e-6,
            "chunked output {k}: {a} vs reference {b}"
        );
    }
}

#[test]
fn reset_matches_fresh_instance() {
    let mut lcg = Lcg(0xDEAD_BEEF_CAFE);
    let seed: Vec<f32> = (0..3_000).map(|_| lcg.next_f32()).collect();
    let probe: Vec<f32> = (0..5_000).map(|_| lcg.next_f32()).collect();

    // Fresh instance on the probe alone.
    let mut fresh = DecimationFilter::new();
    let mut fresh_out = Vec::new();
    fresh.process(&probe, &mut fresh_out);

    // Seeded instance: process the seed, reset, process the probe.
    let mut seeded = DecimationFilter::new();
    let mut scratch = Vec::new();
    seeded.process(&seed, &mut scratch);
    seeded.reset();
    let mut reset_out = Vec::new();
    seeded.process(&probe, &mut reset_out);

    assert_eq!(reset_out.len(), fresh_out.len());
    for (k, (a, b)) in reset_out.iter().zip(fresh_out.iter()).enumerate() {
        assert_eq!(a, b, "reset output {k} differs from a fresh instance");
    }

    // Without the reset the seed's history must still be present (the
    // impulse-seed check the coordinator flush tests rely on): processing
    // an impulse then zeros yields non-zero output only while the impulse
    // is inside the 31-tap window.
    let mut fir = DecimationFilter::new();
    let mut out = Vec::new();
    fir.process(&[1.0f32], &mut out);
    assert_eq!(out.len(), 0, "one input sample yields no output yet");
    fir.process(&vec![0.0f32; 124], &mut out);
    assert_eq!(out.len(), 31);
    assert!(
        out.iter().any(|&s| s != 0.0),
        "impulse must ring through the window before reset"
    );
    fir.reset();
    let mut after_reset = Vec::new();
    fir.process(&vec![0.0f32; 128], &mut after_reset);
    assert_eq!(after_reset.len(), 32);
    assert!(
        after_reset.iter().all(|&s| s == 0.0),
        "reset must remove all previous samples"
    );
}

#[test]
fn output_phase_and_latency() {
    // Output grid: after N inputs exactly floor(N/4) outputs exist, for
    // every remainder 0..4 (checked separately from any signal response).
    for n in [0usize, 1, 2, 3, 4, 5, 7, 8, 9, 127, 128, 129] {
        let mut fir = DecimationFilter::new();
        let mut out = Vec::new();
        fir.process(&vec![0.01f32; n], &mut out);
        assert_eq!(out.len(), n / DECIMATION_FACTOR, "N={n}");
        // Appending one more sample yields exactly one more output only
        // when crossing the 4-boundary; phase is stable across calls.
        fir.process(&[0.01f32], &mut out);
        assert_eq!(out.len(), (n + 1) / DECIMATION_FACTOR, "N={}+1", n);
    }

    // Group delay: an impulse at input index i peaks at output
    // (i + 12)/4 — i.e. 15 input samples (0.3125 ms) of latency.
    assert_eq!(FIR_DELAY_INPUT_SAMPLES, 15);
    assert!((FIR_DELAY_INPUT_SAMPLES as f64 / 48_000.0 - 0.000_312_5).abs() < 1e-12);
    for &i in &[0usize, 8, 40] {
        // Long enough that the peak's output window has fully arrived
        // (output k exists once input 4k+3 has; k_peak = (i+12)/4).
        let mut input = vec![0.0f32; i + 64];
        input[i] = 1.0;
        let mut fir = DecimationFilter::new();
        let mut out = Vec::new();
        fir.process(&input, &mut out);
        let peak = out
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
            .map(|(k, _)| k)
            .expect("non-empty output");
        assert_eq!(
            peak,
            (i + 12) / 4,
            "impulse at input {i} must peak at output {} (found {peak})",
            (i + 12) / 4
        );
        // Retained-index + latency contract: output k's window ends at
        // input 4k+3 and is centered 15 input samples earlier, so an
        // impulse peaks where 4k+3−15 == i.
        assert_eq!(
            4 * peak + 3 - FIR_DELAY_INPUT_SAMPLES,
            i,
            "output {peak} window center must sit at input {i}"
        );
    }
}
