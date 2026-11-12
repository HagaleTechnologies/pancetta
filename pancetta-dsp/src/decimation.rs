//! Unity-gain 4:1 FIR anti-alias decimator for the 48 kHz → 12 kHz
//! receive path (PAN-320).
//!
//! The live DSP stage historically subsampled 48 kHz audio by keeping every
//! 4th sample with no filter. Everything the rig or an SDR delivers above
//! 6 kHz then folds back into the 12 kHz output unattenuated (worst case
//! ~6 dB of folded noise); a strong 9.5 kHz birdie lands exactly on a
//! 2.5 kHz FT8 signal. The removed historical "65-tap FIR" attenuated
//! in-band signals (86 vs 1 decodes) because it was mis-specified, not
//! because filtering hurts; this module is the measured replacement, and
//! `tests/decimation.rs` pins its passband so that failure mode cannot
//! recur.
//!
//! Design (mirrors the clean-room Kaiser recipe in
//! `pancetta-ft8/src/baseband.rs:64-133`, re-instantiated at fs = 48 kHz):
//!
//! * Kaiser-windowed sinc, **31 symmetric taps**, β = `kaiser_beta(45 dB)`
//!   ≈ 3.9754, cutoff at the 4000/8000 Hz midpoint (6000 Hz), normalized
//!   to unity DC gain.
//! * This is a *quarter-band* low-pass for 4:1 decimation, not a 2:1
//!   half-band FIR: the FT8 decoder scans candidates to 4 kHz, so the
//!   passband stays flat through 4 kHz, the transition sits in the
//!   decode-safe 4–8 kHz fold zone, and the stopband starts at 8 kHz
//!   (the first region that aliases onto the 0–4 kHz decode band).
//! * Measured response (10 Hz grid): passband −0.082…+0.041 dB,
//!   stopband floor ≤ −39.4 dB over 8–24 kHz; group delay 15 input
//!   samples (0.3125 ms).
//!
//! No rate parameter: this filter is specifically 48 kHz → 12 kHz. Other
//! supported input rates keep the existing subsample path in the
//! coordinator.
//!
//! Streaming contract: `process` appends decimated outputs to the
//! caller's buffer (it never clears it), keeps history across calls, and
//! emits output `k` — `y[k] = Σ h[j]·x[4k+3−j]` with negative indices
//! treated as zero — exactly when input sample `4k+3` arrives, so after
//! `N` total inputs `⌊N/4⌋` outputs exist and the retained grid matches
//! the subsampler's indices 3, 7, 11, …

/// Input sample rate this filter is designed for (Hz).
pub const DECIMATION_INPUT_RATE_HZ: f64 = 48_000.0;

/// Output sample rate (Hz): 48 kHz / 4.
pub const DECIMATION_OUTPUT_RATE_HZ: f64 = 12_000.0;

/// Decimation factor this filter implements.
pub const DECIMATION_FACTOR: usize = 4;

/// Passband edge (Hz): the FT8 decoder scans candidates to 4 kHz, so the
/// passband must stay flat through here.
const FIR_PASS_HZ: f64 = 4_000.0;

/// Stopband edge (Hz): the first alias region that lands on the 0–4 kHz
/// decode band starts here.
const FIR_STOP_HZ: f64 = 8_000.0;

/// Kaiser design attenuation (dB). Yields exactly 31 taps with the 4 kHz
/// transition at 48 kHz — the "~31 taps" the ticket names.
const FIR_ATTEN_DB: f64 = 45.0;

/// Number of taps in the shipped filter (31, symmetric Type-I FIR).
pub const FIR_TAPS: usize = 31;

/// Group delay in input samples: `(taps − 1) / 2` = 15 = 0.3125 ms at
/// 48 kHz. [`DecimationFilter::delay_input_samples`] reports it so the
/// coordinator can compensate window timestamps.
pub const FIR_DELAY_INPUT_SAMPLES: usize = (FIR_TAPS - 1) / 2;

/// Modified Bessel function of the first kind, order 0 — for the Kaiser
/// window. Series form (mirrors `pancetta-ft8/src/baseband.rs`).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half_x = x / 2.0;
    for k in 1..50 {
        term *= (half_x / k as f64).powi(2);
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

/// Kaiser β from a target stopband attenuation (Kaiser's empirical formula).
fn kaiser_beta(atten_db: f64) -> f64 {
    if atten_db > 50.0 {
        0.1102 * (atten_db - 8.7)
    } else if atten_db >= 21.0 {
        0.5842 * (atten_db - 21.0).powf(0.4) + 0.07886 * (atten_db - 21.0)
    } else {
        0.0
    }
}

/// Kaiser tap count from attenuation and transition width, forced odd for a
/// symmetric (linear-phase) Type-I FIR.
fn kaiser_num_taps(atten_db: f64, transition_hz: f64, fs: f64) -> usize {
    let n = ((atten_db - 8.0) / (2.285 * 2.0 * std::f64::consts::PI * transition_hz / fs)).ceil()
        as usize;
    if n.is_multiple_of(2) {
        n + 1
    } else {
        n
    }
}

/// Design the 31-tap quarter-band decimation low-pass in f64: Kaiser-windowed
/// sinc, cutoff at the pass/stop midpoint (6 kHz), unity DC gain.
///
/// Exposed so tests verify the response of the *shipped* coefficients and
/// the research A/B tool can serialize them into its report. Deterministic.
pub fn design_fir31_taps() -> [f64; FIR_TAPS] {
    let fc = 0.5 * (FIR_PASS_HZ + FIR_STOP_HZ);
    let transition = FIR_STOP_HZ - FIR_PASS_HZ;
    let beta = kaiser_beta(FIR_ATTEN_DB);
    let num_taps = kaiser_num_taps(FIR_ATTEN_DB, transition, DECIMATION_INPUT_RATE_HZ);
    debug_assert_eq!(num_taps, FIR_TAPS, "PAN-320 design constants drifted");
    let m = (num_taps - 1) as f64 / 2.0;
    let i0_beta = bessel_i0(beta);
    let wc = 2.0 * fc / DECIMATION_INPUT_RATE_HZ;

    let mut taps = [0.0f64; FIR_TAPS];
    for (n, tap) in taps.iter_mut().enumerate() {
        let t = n as f64 - m;
        let sinc = if t.abs() < 1e-9 {
            wc
        } else {
            (std::f64::consts::PI * wc * t).sin() / (std::f64::consts::PI * t)
        };
        let r = t / m;
        let win = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
        *tap = sinc * win;
    }
    let dc: f64 = taps.iter().sum();
    for tap in &mut taps {
        *tap /= dc;
    }
    taps
}

/// Streaming 48 kHz → 12 kHz anti-alias decimator (31-tap unity-gain FIR).
///
/// See the [module](self) docs for the design and the streaming contract.
#[derive(Debug, Clone)]
pub struct DecimationFilter {
    /// Coefficients in *newest-first* order: `coeffs[j]` multiplies the
    /// input sample `j` positions behind the newest one. Symmetric, so this
    /// equals the natural order, but the processing loop does not rely on
    /// that (it pairs `coeffs[j]` with the window explicitly).
    coeffs: [f32; FIR_TAPS],
    /// The same design in f64, for response testing / report serialization.
    taps_f64: [f64; FIR_TAPS],
    /// Rolling sample store: `buf[0]` holds absolute input index
    /// `base_abs`. Pre-filled with `FIR_TAPS − 1` zeros so the first
    /// outputs see the zero-padded initial history the reference defines.
    buf: Vec<f32>,
    /// Absolute input index of `buf[0]` (starts negative: the zero pad).
    base_abs: isize,
    /// Total input samples processed since construction/reset.
    total: usize,
    /// Outputs already emitted (`⌊total/4⌋` after every `process` call).
    emitted: usize,
}

impl Default for DecimationFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl DecimationFilter {
    /// Construct the filter with zero initial history (the documented
    /// zero-initialized-history convention; the first-sample transient is
    /// part of the contract).
    pub fn new() -> Self {
        let taps_f64 = design_fir31_taps();
        let mut coeffs = [0.0f32; FIR_TAPS];
        for (c, t) in coeffs.iter_mut().zip(taps_f64.iter()) {
            *c = *t as f32;
        }
        Self {
            coeffs,
            taps_f64,
            buf: vec![0.0; FIR_TAPS - 1],
            base_abs: -((FIR_TAPS - 1) as isize),
            total: 0,
            emitted: 0,
        }
    }

    /// The shipped FIR coefficients (exact f64 design values), natural tap
    /// order `h[0..31]` (oldest first). For response testing and report
    /// serialization; the processing path uses the f32 rounding of the same
    /// numbers, and the reference tests cover that rounding.
    pub fn coefficients(&self) -> &[f64; FIR_TAPS] {
        &self.taps_f64
    }

    /// Group delay in input samples (15 = 0.3125 ms at 48 kHz).
    pub fn delay_input_samples(&self) -> usize {
        FIR_DELAY_INPUT_SAMPLES
    }

    /// Decimation factor (4).
    pub fn factor(&self) -> usize {
        DECIMATION_FACTOR
    }

    /// Process a batch of arbitrary length, appending one output per four
    /// inputs to `output`. Never clears the caller's buffer.
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        self.buf.extend_from_slice(input);
        self.total += input.len();
        let kmax = self.total / DECIMATION_FACTOR;
        output.reserve(kmax - self.emitted);
        for k in self.emitted..kmax {
            // Output k's newest input is absolute index 4k+3.
            let n_abs = (DECIMATION_FACTOR * k + (DECIMATION_FACTOR - 1)) as isize;
            let n = (n_abs - self.base_abs) as usize; // index in self.buf
                                                      // Window start (absolute 4k−27) is never before the zero pad:
                                                      // the store opens with FIR_TAPS−1 zeros and compaction keeps
                                                      // every index the next output still needs.
            let start_abs = n_abs - (FIR_TAPS as isize - 1);
            let start = (start_abs - self.base_abs).max(0) as usize;
            let w = &self.buf[start..=n];
            output.push(dot_newest_first(&self.coeffs, w));
        }
        self.emitted = kmax;
        // Compact: keep the last 34 samples (31-tap window + phase slack) —
        // 34 covers the oldest index (4·emitted − 27) the next output needs
        // for every phase 0..4. One drain per call, not per output.
        if self.buf.len() > 64 {
            let cut = self.buf.len() - 34;
            self.buf.drain(..cut);
            self.base_abs += cut as isize;
        }
    }

    /// Drop all history and phase: the next `process` behaves exactly like
    /// a freshly constructed filter. Called at every destructive DSP
    /// buffer flush (gain change, dial change, mode change, clock jump);
    /// ordinary overlap retention between windows is *not* a reset.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.buf.resize(FIR_TAPS - 1, 0.0);
        self.base_abs = -((FIR_TAPS - 1) as isize);
        self.total = 0;
        self.emitted = 0;
    }
}

/// One output sample: dot product of newest-first coefficients with a
/// 31-sample window whose *last* element is the newest input,
/// `y = Σ coeffs[j] · w[30−j]`.
///
/// The reduction runs in four independent lanes over
/// `chunks_exact(4)`/`rchunks_exact(4)` so LLVM vectorizes it without
/// fast-math flags: FP reordering stays within each lane, which the
/// streaming-vs-reference tolerance (1e-6) absorbs. The 3-sample tail
/// (oldest window end / newest coefficient end) joins the lanes directly.
// `chunks_exact(4)` pairs with `rchunks_exact(4)` below; the forward
// `as_chunks` suggestion has no reverse counterpart, so keep the iterator
// form for the symmetric pairing.
#[inline]
#[allow(clippy::chunks_exact_to_as_chunks)]
fn dot_newest_first(coeffs: &[f32; FIR_TAPS], w: &[f32]) -> f32 {
    debug_assert_eq!(w.len(), FIR_TAPS);
    let mut a0 = 0.0f32;
    let mut a1 = 0.0f32;
    let mut a2 = 0.0f32;
    let mut a3 = 0.0f32;
    // coeffs[j] pairs with w[30−j]. coeffs chunk [4i..4i+4] pairs with the
    // reversed window chunk w[27−4i .. 31−4i]: within an rchunk [x0,x1,x2,x3]
    // (ascending index), coeffs[4i+m] pairs with x[3−m].
    for (c, x) in coeffs.chunks_exact(4).zip(w.rchunks_exact(4)) {
        a0 += c[0] * x[3];
        a1 += c[1] * x[2];
        a2 += c[2] * x[1];
        a3 += c[3] * x[0];
    }
    // Remainder: coeffs[28..31] pair with w[2], w[1], w[0] (the three
    // oldest samples = the rchunks remainder, ascending index order).
    let rc = coeffs.chunks_exact(4).remainder();
    let rx = w.rchunks_exact(4).remainder();
    a0 += rc[0] * rx[2];
    a1 += rc[1] * rx[1];
    a2 += rc[2] * rx[0];
    (a0 + a1) + (a2 + a3)
}
