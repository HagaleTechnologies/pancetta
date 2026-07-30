//! Synth corpus manifest format. The manifest is the canonical source
//! of truth for synth fixtures: an entry lists the encoded message text,
//! the target SNR (dB), the channel impairments applied, and the WAV
//! path. Regenerating from manifest + seed produces byte-identical WAVs.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum SynthChannel {
    /// AWGN only — additive white Gaussian noise at the target SNR.
    Awgn,
    /// AWGN + slow frequency drift (linear, configurable Hz/s).
    AwgnDrift,
    // Future: Watterson channel model (Doppler + multipath fading).
    // Not in Plan 2; leave as enum extension point.
}

/// Top-level synth corpus config — the input to the gen-synth binary.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SynthConfig {
    pub schema_version: u32,
    pub label: String,
    /// Messages to encode. Each will be modulated at every snr_db level
    /// listed, producing `messages.len() * snr_steps.len()` total WAVs
    /// for AWGN, or `* drift_steps.len()` more for AwgnDrift.
    pub messages: Vec<String>,
    pub snr_steps_db: Vec<f64>,
    pub channel: SynthChannel,
    /// Drift rates in Hz/s applied to each WAV when channel=AwgnDrift.
    /// Ignored for Awgn. Empty means [0.0]. Crude model — multiplicative
    /// cosine on the real signal, not true Doppler frequency translation.
    /// Sufficient as a hb-015 unblock; rigorous Watterson is future work.
    #[serde(default)]
    pub drift_steps_hz_per_sec: Vec<f64>,
    /// Deterministic seed; same seed + same config → byte-identical output.
    pub seed: u64,
    /// Output dir relative to workspace root. WAVs land here.
    pub output_dir: PathBuf,
    /// Task W0.4 (2026-07-07): which digital mode to generate/modulate
    /// this corpus as. `#[serde(default)]` → `Mode::Ft8`, so every
    /// pre-W0.4 committed config (which has no `mode` key at all) still
    /// deserializes as FT8 — the mode this whole corpus format was built
    /// for before FT4 support landed.
    #[serde(default)]
    pub mode: crate::Mode,
}

/// One generated WAV entry — the unit of synth ground truth.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SynthEntry {
    pub wav_path: PathBuf,
    pub encoded_message: String,
    pub snr_db: f64,
    pub channel: SynthChannel,
    /// Drift rate in Hz/s (only meaningful when channel=AwgnDrift; 0 otherwise).
    #[serde(default)]
    pub drift_hz_per_sec: f64,
    pub seed_for_this_wav: u64,
    /// Task W0.2 (2026-07-06): base audio frequency the tones were
    /// modulated at, in Hz. `#[serde(default)]` to `BASE_FREQUENCY_HZ_LEGACY`
    /// (1500.0) so older committed manifests (doppler, synth-pair) that
    /// predate per-file frequency randomization still deserialize —
    /// and that default is not an arbitrary placeholder, it is the exact
    /// fixed frequency those older corpora were actually generated at.
    #[serde(default = "legacy_base_freq_hz")]
    pub base_freq_hz: f64,
    /// Task W0.2: per-file time offset (seconds) the signal was placed at
    /// within its slot buffer, relative to the nominal lead-in. `0.0`
    /// default matches every pre-W0.2 corpus (no lead-in padding, signal
    /// started at sample 0 of the WAV).
    #[serde(default)]
    pub dt_s: f64,
}

/// Legacy fixed base frequency used by every synth WAV before Task W0.2
/// added per-file frequency randomization (`gen_synth::modulate_message_at`
/// default in the original `src/bin/gen_synth.rs`).
fn legacy_base_freq_hz() -> f64 {
    1500.0
}

/// Manifest = config + populated entries. Written after gen-synth runs.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SynthManifest {
    pub schema_version: u32,
    pub config: SynthConfig,
    pub entries: Vec<SynthEntry>,
}

impl SynthConfig {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;
}

impl SynthManifest {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    pub fn save<P: AsRef<Path>>(&self, path: P) -> anyhow::Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let m: SynthManifest = serde_json::from_str(&s)?;
        anyhow::ensure!(
            m.schema_version == Self::CURRENT_SCHEMA_VERSION,
            "SynthManifest schema_version {} not supported (expected {})",
            m.schema_version,
            Self::CURRENT_SCHEMA_VERSION,
        );
        Ok(m)
    }
}

// ---------------------------------------------------------------------------
// Pair-synth corpus (hb-146 — synthetic adversarial mutual-masking pairs).
//
// Each WAV contains TWO FT8 signals at controllable offsets:
//   - delta_snr_db:   strength difference between signal_a and signal_b
//   - delta_freq_hz:  frequency separation between the two signals
//   - delta_time_s:   time offset of signal_b relative to signal_a
// The base signal_a is placed at the canonical 1500 Hz; signal_b is offset
// by delta_freq_hz. Both are mixed into a 15 s slot buffer with AWGN at
// `strong_snr_db` (the SNR of the *strong* signal vs noise).
//
// V2/V3 of hb-086 (soft cancellation, sync relaxation) shelved because
// pancetta's decoded neighbors on hard-200 were uniformly STRONG — no
// marginal-SNR pairs to exercise the joint-decoding mechanisms. This
// corpus generates such pairs on demand at controlled (deltaSNR, deltaF,
// deltaT) grid points.
// ---------------------------------------------------------------------------

/// Pair-synth corpus config — input to the gen-synth-pair binary.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SynthPairConfig {
    pub schema_version: u32,
    pub label: String,
    /// Base message templates. Each will be paired with itself rotated
    /// (msg_strong = templates[i], msg_weak = templates[(i+1) % len]).
    pub message_templates: Vec<String>,
    /// SNR of the *strong* (higher-power) signal relative to the AWGN noise
    /// floor, in dB. Drives absolute noise level. -10 dB is a typical
    /// decodable strong signal in pancetta synth-clean.
    pub strong_snr_db: f64,
    /// Strength delta in dB between the strong and weak signals. ΔSNR=0
    /// means equal strength; ΔSNR=12 means the weak signal is 12 dB
    /// below the strong one.
    pub delta_snr_db_steps: Vec<f64>,
    /// Frequency separation between weak and strong, in Hz. Positive
    /// values place the weak signal above the strong one in frequency.
    pub delta_freq_hz_steps: Vec<f64>,
    /// Time offset of weak relative to strong, in seconds.
    pub delta_time_s_steps: Vec<f64>,
    /// Lead-in silence at the start of the 15 s slot, in seconds. Gives
    /// negative delta_time_s headroom. Default 1.0 s.
    #[serde(default = "default_slot_lead_in_s")]
    pub slot_lead_in_s: f64,
    /// Maximum number of generated WAVs (subsample after grid expansion).
    /// 0 means "no cap".
    #[serde(default)]
    pub max_wavs: usize,
    /// Deterministic seed.
    pub seed: u64,
    /// Output dir relative to workspace root.
    pub output_dir: PathBuf,
}

fn default_slot_lead_in_s() -> f64 {
    1.0
}

/// One generated pair WAV entry.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SynthPairEntry {
    pub wav_path: PathBuf,
    /// The two encoded messages present in this WAV.
    pub message_strong: String,
    pub message_weak: String,
    /// SNR of the strong signal vs noise (dB).
    pub strong_snr_db: f64,
    /// Strength delta between strong and weak (dB).
    pub delta_snr_db: f64,
    /// Frequency separation (Hz). Strong sits at 1500 Hz, weak at
    /// 1500 + delta_freq_hz.
    pub delta_freq_hz: f64,
    /// Time offset of weak signal relative to strong (s).
    pub delta_time_s: f64,
    pub seed_for_this_wav: u64,
}

/// Pair-synth manifest = config + entries.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SynthPairManifest {
    pub schema_version: u32,
    pub config: SynthPairConfig,
    pub entries: Vec<SynthPairEntry>,
}

impl SynthPairConfig {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;
}

impl SynthPairManifest {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    pub fn save<P: AsRef<Path>>(&self, path: P) -> anyhow::Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let m: SynthPairManifest = serde_json::from_str(&s)?;
        anyhow::ensure!(
            m.schema_version == Self::CURRENT_SCHEMA_VERSION,
            "SynthPairManifest schema_version {} not supported (expected {})",
            m.schema_version,
            Self::CURRENT_SCHEMA_VERSION,
        );
        Ok(m)
    }
}

// ---------------------------------------------------------------------------
// Synth WAV generation core (Task W0.2, 2026-07-06 — 2500 Hz SNR
// calibration + real sensitivity curve).
//
// Moved out of `src/bin/gen_synth.rs` and into the library so
// `tests/snr_calibration_tests.rs` can exercise the noise-scaling formula
// directly against a real generated + written + re-read WAV (mirrors the
// `gen_noise.rs` / `bin/gen_noise.rs` split from Task W0.1). The
// `gen-synth` binary is now a thin CLI wrapper around these functions.
// ---------------------------------------------------------------------------

use pancetta_ft8::{
    Ft8Encoder, Ft8Modulator, ProtocolParams, NUM_SYMBOLS, SAMPLE_RATE, WINDOW_SAMPLES,
};
use rand::rngs::StdRng;
use rand::SeedableRng;
use rand_distr::{Distribution, Normal};

/// Number of audio samples per FT8 symbol (0.16 s at 12 kHz = 1920
/// samples). Derived from `pancetta_ft8`'s own timing constants
/// (`WINDOW_SAMPLES / NUM_SYMBOLS`) rather than hardcoded, so it can never
/// drift out of sync with the codec if those constants ever change.
pub const SAMPLES_PER_SYMBOL: usize = WINDOW_SAMPLES / NUM_SYMBOLS;

/// Clean-signal modulation amplitude (`Ft8Modulator`'s `tx_power` arg)
/// used by every synth WAV.
///
/// **Not 1.0 (full scale).** The pre-W0.2 generator modulated at
/// `tx_power = 1.0` and then derived `noise_rms` as a multiple of
/// `signal_rms` — fine at mild SNRs, but at the very negative SNRs this
/// corpus's sensitivity curve actually needs (down to −24 dB in the 2500
/// Hz reference-bandwidth convention), the required noise RMS is *many
/// times* the ±1.0 full-scale range (e.g. `noise_rms ≈ 16.5` at −24 dB
/// with `tx_power = 1.0`), so `[-1.0, 1.0]` clamping before 16-bit
/// quantization destructively clips the overwhelming majority of samples.
/// That clipping was caught by this task's own calibration test: it
/// non-linearly compresses the waveform, which measurably suppresses the
/// coherent tone-bin signal power relative to the noise floor (a −1.7 dB
/// discrepancy was observed at the −15 dB test point before this fix — and
/// confirmed to persist *unchanged* when only `Ft8Modulator`'s `tx_power`
/// constructor argument was lowered, which is what exposed the
/// peak-normalization behavior below: `tx_power` alone has **zero** effect
/// on `modulate_symbols`'s returned amplitude).
///
/// **Applied as an explicit post-hoc multiply, not via `tx_power`.**
/// `Ft8Modulator::apply_final_processing` (called internally by
/// `modulate_symbols`; designed for real transmission, where you always
/// want full on-air headroom) unconditionally **peak-normalizes its
/// output to 0.95** regardless of the `tx_power` value passed to
/// `Ft8Modulator::new` — so [`modulate_message_at`] scales the returned
/// buffer down itself, after the library's forced normalization.
///
/// Scaling the clean signal down to `0.005` (of its post-normalization
/// ~0.67 RMS, i.e. an effective final RMS of ~0.0034) keeps even the
/// worst-case (−24 dB) noise RMS at ~6σ ≈ 0.49 — comfortably inside
/// `[-1.0, 1.0]` with a wide safety margin, matching how real off-air
/// recordings/this crate's own noise-floor reference level
/// (`gen_noise::TARGET_NOISE_RMS = 0.03`, Task W0.1) sit well below full
/// scale. Absolute amplitude doesn't affect decodability (the decoder
/// path is scale-invariant); only the signal:noise *ratio* — which this
/// constant does not change — matters for the sensitivity curve.
const SYNTH_SIGNAL_SCALE: f64 = 0.005;

/// Encode an FT8 message to its 79 tone-symbol values (0..NUM_TONES),
/// without modulating audio. Exposed so calibration/measurement code can
/// know exactly which tone each symbol carries (needed to locate the
/// per-symbol tone bin when independently measuring signal power from a
/// generated WAV).
pub fn encode_message_symbols(text: &str) -> anyhow::Result<[u8; NUM_SYMBOLS]> {
    let mut encoder = Ft8Encoder::new();
    encoder
        .encode_message(text, None)
        .map_err(|e| anyhow::anyhow!("Ft8Encoder::encode_message failed for '{text}': {e}"))
}

/// Encode + modulate one FT8 message into 12 kHz mono f32 samples at a
/// caller-chosen base audio frequency, scaled to [`SYNTH_SIGNAL_SCALE`]
/// (see that constant's doc for why).
///
/// Uses the real pancetta-ft8 public API (behind the `transmit` feature):
///   1. `Ft8Encoder::new().encode_message(text, None)` → `[u8; 79]` tone symbols
///   2. `Ft8Modulator::new(sample_rate, base_freq_hz, tx_power).modulate_symbols(&symbols, 0.0)`
///
/// `frequency_offset` is passed as `0.0` because the modulator's
/// `base_frequency` constructor argument already places the tones at
/// `base_freq_hz`. `tx_power` is passed as `1.0` — it has no effect on
/// the actual output amplitude (see [`SYNTH_SIGNAL_SCALE`]'s doc), so an
/// arbitrary in-range value is used and the real scaling is applied below.
pub fn modulate_message_at(text: &str, base_freq_hz: f64) -> anyhow::Result<Vec<f32>> {
    modulate_message_at_protocol(text, base_freq_hz, &ProtocolParams::ft8())
}

/// Protocol-generic sibling of [`modulate_message_at`] (Task W0.4,
/// 2026-07-07). Encode + modulate one message for ANY [`ProtocolParams`]
/// (FT8, FT4, or FT2) via the protocol-aware `Ft8Encoder::with_protocol` /
/// `encode_message_protocol` / `Ft8Modulator::modulate_symbols_protocol`
/// API — the same pattern already exercised by
/// `pancetta-ft8/tests/round_trip_tests.rs`'s FT4 round-trip tests.
///
/// `modulate_message_at` is now a thin wrapper calling this with
/// `ProtocolParams::ft8()`, so the FT8 corpus path is unchanged (verified
/// by the existing `modulate_message_at_respects_base_freq` /
/// `snr_calibration_tests.rs` tests, which exercise `modulate_message_at`
/// and continue to pass byte-identically): `encode_message_protocol` /
/// `generate_symbols_protocol` compute the exact same symbol sequence as
/// `encode_message` / `generate_symbols` for FT8 (no XOR scrambling,
/// `bits_per_symbol == 3`, same Costas table) — only the fixed-array vs
/// `Vec` return type differs.
pub fn modulate_message_at_protocol(
    text: &str,
    base_freq_hz: f64,
    params: &ProtocolParams,
) -> anyhow::Result<Vec<f32>> {
    let mut encoder = Ft8Encoder::with_protocol(params.clone());
    let symbols = encoder.encode_message_protocol(text, None).map_err(|e| {
        anyhow::anyhow!("Ft8Encoder::encode_message_protocol failed for '{text}': {e}")
    })?;

    let mut modulator = Ft8Modulator::new(SAMPLE_RATE, base_freq_hz, 1.0)
        .map_err(|e| anyhow::anyhow!("Ft8Modulator::new failed: {e}"))?;
    let mut samples = modulator
        .modulate_symbols_protocol(&symbols, 0.0, params)
        .map_err(|e| {
            anyhow::anyhow!("Ft8Modulator::modulate_symbols_protocol failed for '{text}': {e}")
        })?;

    for s in samples.iter_mut() {
        *s *= SYNTH_SIGNAL_SCALE as f32;
    }

    Ok(samples)
}

/// Apply crude linear frequency drift to a real signal by multiplicative
/// time-varying cosine. NOT true Doppler — true Doppler requires complex
/// analytic signal manipulation (Hilbert transform). This multiplicative
/// model perturbs the spectrogram (introduces AM-like sidebands and shifts
/// peak energy across bins as time progresses) which is sufficient as a
/// hb-015 unblock corpus. Rigorous Doppler evaluation needs a Watterson
/// channel implementation in a future iter. (Moved unchanged from
/// `src/bin/gen_synth.rs`, Task W0.2.)
pub fn apply_linear_drift_crude(samples: &mut [f32], drift_hz_per_sec: f64) {
    if drift_hz_per_sec.abs() < f64::EPSILON {
        return;
    }
    let dt = 1.0 / SAMPLE_RATE as f64;
    for (i, s) in samples.iter_mut().enumerate() {
        let t = i as f64 * dt;
        // Phase ramp: integral of 2π × drift × t dt = π × drift × t²
        let phase = std::f64::consts::PI * drift_hz_per_sec * t * t;
        *s = (*s as f64 * phase.cos()) as f32;
    }
}

/// RMS (root-mean-square) amplitude of a sample buffer. `0.0` for an empty
/// buffer (rather than dividing by zero).
pub fn signal_rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / samples.len() as f64).sqrt()
}

/// Mix additive white Gaussian noise (AWGN) into `samples` at a target SNR,
/// following the **WSJT-X 2500 Hz reference-bandwidth convention**.
///
/// WSJT-X / `jt9` report decode SNR relative to a fixed 2500 Hz reference
/// noise bandwidth, not the full audio Nyquist band (see Franke & Taylor,
/// "The FT4 and FT8 Communication Protocols", QEX 2020, SNR-reporting
/// section; and this repo's own
/// `docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md`
/// section 2). For white noise spanning the full `[0, sample_rate/2]`
/// band, only a `2500 / (sample_rate/2)` fraction of the total noise power
/// actually falls inside that 2500 Hz reference sub-band. So to hit a
/// target reference-band SNR of `snr_db`, the *total*-band noise RMS must
/// be **louder** than a naive full-band SNR calculation would produce, by
/// `sqrt((sample_rate/2) / 2500)`:
///
/// ```text
/// noise_rms = signal_rms / 10^(snr_db / 20) * sqrt(full_band_hz / 2500.0)
/// ```
///
/// At 12 kHz (`full_band_hz = 6000`), that factor is `sqrt(6000/2500) ≈
/// 1.549` (+3.8 dB more noise than the pre-W0.2 generator injected for the
/// same nominal label). Same convention independently re-derived in
/// `examples/batch62_soft_combiner_repeats.rs::sigma_for_snr_db`.
///
/// `signal_rms_val` must be measured from the *clean* (pre-noise,
/// pre-padding) modulated signal — see [`signal_rms`] — not from a
/// zero-padded slot buffer, or the estimate is diluted by silence.
pub fn add_awgn_2500hz_ref(samples: &mut [f32], signal_rms_val: f64, snr_db: f64, rng_seed: u64) {
    /// WSJT-X's fixed SNR reference bandwidth, in Hz.
    const REFERENCE_BANDWIDTH_HZ: f64 = 2500.0;
    let full_band_hz = SAMPLE_RATE as f64 / 2.0; // Nyquist (6000 Hz @ 12 kHz)
    let mut rng = StdRng::seed_from_u64(rng_seed);
    let noise_rms =
        signal_rms_val / 10f64.powf(snr_db / 20.0) * (full_band_hz / REFERENCE_BANDWIDTH_HZ).sqrt();
    let normal = Normal::new(0.0_f64, noise_rms).expect("noise stddev must be finite");
    for s in samples.iter_mut() {
        *s += normal.sample(&mut rng) as f32;
    }
}

/// Place a (possibly drift-perturbed) clean signal inside a longer "slot"
/// buffer of zeros, with the signal starting at `lead_in_s + dt_s` seconds
/// from the buffer's start.
///
/// Gives synth WAVs realistic silence padding plus a per-file randomized
/// `dt` (decode-time offset), so a decoder under test cannot overfit a
/// single fixed grid position — Task W0.2: before this, every synth WAV
/// placed the signal at exactly sample 0 (implicit `dt = 0`) with no
/// lead-in silence at all.
///
/// Panics only via out-of-bounds silently clipping (not panicking): any
/// portion of `signal` that would land outside `[0, slot_len_samples)` is
/// dropped rather than causing an index panic, so a caller-chosen
/// `dt_s`/`lead_in_s` combination that doesn't fit degrades safely instead
/// of crashing the whole corpus generation run.
pub fn place_in_slot(
    signal: &[f32],
    dt_s: f64,
    lead_in_s: f64,
    slot_len_samples: usize,
) -> Vec<f32> {
    let mut buf = vec![0.0f32; slot_len_samples];
    let start_s = lead_in_s + dt_s;
    let start_sample = (start_s * SAMPLE_RATE as f64).round() as i64;
    for (i, &s) in signal.iter().enumerate() {
        let idx = start_sample + i as i64;
        if idx >= 0 && (idx as usize) < buf.len() {
            buf[idx as usize] += s;
        }
    }
    buf
}

#[cfg(test)]
mod generation_tests {
    use super::*;

    #[test]
    fn signal_rms_of_dc_one_is_one() {
        let s = vec![1.0f32; 100];
        assert!((signal_rms(&s) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn signal_rms_of_empty_is_zero() {
        assert_eq!(signal_rms(&[]), 0.0);
    }

    #[test]
    fn place_in_slot_positions_signal_at_expected_offset() {
        let signal = vec![1.0f32; 10];
        let buf = place_in_slot(&signal, 0.0, 1.0, 20_000); // lead_in 1.0s @ 12kHz = 12000
        assert!(buf[..12_000].iter().all(|&s| s == 0.0));
        assert!(buf[12_000..12_010].iter().all(|&s| s == 1.0));
        assert!(buf[12_010..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn place_in_slot_honors_dt_offset() {
        let signal = vec![1.0f32; 10];
        // dt = +0.1s @ 12kHz = 1200 extra samples beyond the 1.0s lead-in.
        let buf = place_in_slot(&signal, 0.1, 1.0, 20_000);
        assert!(buf[13_200..13_210].iter().all(|&s| s == 1.0));
        assert_eq!(buf[13_199], 0.0);
        assert_eq!(buf[13_210], 0.0);
    }

    #[test]
    fn modulate_message_at_respects_base_freq() {
        // Sanity: modulating at two different base frequencies produces
        // different (non-identical) audio.
        let a = modulate_message_at("CQ K1ABC FN42", 800.0).unwrap();
        let b = modulate_message_at("CQ K1ABC FN42", 2200.0).unwrap();
        assert_eq!(a.len(), b.len());
        assert_ne!(a, b);
    }

    #[test]
    fn encode_message_symbols_matches_modulator_input_len() {
        let symbols = encode_message_symbols("CQ K1ABC FN42").unwrap();
        assert_eq!(symbols.len(), NUM_SYMBOLS);
        assert!(symbols.iter().all(|&s| s < 8));
    }

    /// Task W0.4: `modulate_message_at` is now a thin wrapper over
    /// `modulate_message_at_protocol(.., &ProtocolParams::ft8())`. This
    /// guards the refactor: FT8 output must stay byte-identical to
    /// calling the protocol-generic path directly, since every other
    /// FT8 synth test (`modulate_message_at_respects_base_freq`,
    /// `snr_calibration_tests.rs`) depends on `modulate_message_at`'s
    /// exact output being unchanged.
    #[test]
    fn modulate_message_at_matches_protocol_path_for_ft8() {
        let via_wrapper = modulate_message_at("CQ K1ABC FN42", 1500.0).unwrap();
        let via_protocol =
            modulate_message_at_protocol("CQ K1ABC FN42", 1500.0, &ProtocolParams::ft8()).unwrap();
        assert_eq!(via_wrapper, via_protocol);
    }

    /// Task W0.4: FT4 modulation produces a well-formed, distinctly
    /// shorter signal than FT8 for the same message (105 symbols @
    /// 0.048s/symbol = 5.04s vs FT8's 79 @ 0.16s = 12.64s), and respects
    /// base frequency like the FT8 path.
    #[test]
    fn modulate_message_at_protocol_ft4_produces_shorter_signal_than_ft8() {
        let ft4_params = ProtocolParams::ft4();
        let ft4 = modulate_message_at_protocol("CQ K1ABC FN42", 1500.0, &ft4_params).unwrap();
        let ft8 = modulate_message_at("CQ K1ABC FN42", 1500.0).unwrap();

        // FT4 total_samples: 105 symbols * (0.048s * 12000 Hz) = 60_480.
        assert_eq!(ft4.len(), ft4_params.total_samples(SAMPLE_RATE));
        assert!(
            ft4.len() < ft8.len(),
            "FT4 signal ({} samples) should be shorter than FT8 ({} samples)",
            ft4.len(),
            ft8.len()
        );

        let a = modulate_message_at_protocol("CQ K1ABC FN42", 800.0, &ft4_params).unwrap();
        let b = modulate_message_at_protocol("CQ K1ABC FN42", 2200.0, &ft4_params).unwrap();
        assert_eq!(a.len(), b.len());
        assert_ne!(a, b);
    }

    // ---------------------------------------------------------------------
    // AWGN statistical invariants (PAN-1).
    //
    // `add_awgn_2500hz_ref` is the crate's primary noise-injection path and
    // had no test at all. These four assert *distribution* properties — RMS
    // level, SNR scaling, zero mean, per-seed reproducibility — never the
    // RNG byte stream, so they hold on both sides of a `rand` major bump and
    // are what distinguishes "the migration preserved behavior" from "the
    // migration compiled". See `docs/DECISIONS/` / PAN-1 plan Phase 1.
    // ---------------------------------------------------------------------

    /// The WSJT-X 2500 Hz reference-bandwidth convention inflates the injected
    /// noise RMS by sqrt(full_band / 2500) over a naive full-band calculation.
    /// At 12 kHz that is sqrt(6000/2500) ≈ 1.5492. See `add_awgn_2500hz_ref`.
    #[test]
    fn awgn_2500hz_ref_hits_reference_bandwidth_rms() {
        // 10 s @ 12 kHz. Relative std err of an RMS estimate is ≈ 1/sqrt(2n)
        // ≈ 0.2%, so the 2% tolerance below is ~10σ — loose enough never to
        // flake, tight enough to catch a wrong bandwidth factor (the nearest
        // wrong answer, dropping the correction entirely, is 35% low).
        let n = 120_000;
        let mut buf = vec![0.0_f32; n];
        add_awgn_2500hz_ref(&mut buf, 1.0, 0.0, 42);
        let measured = signal_rms(&buf);
        let expected = (6000.0_f64 / 2500.0).sqrt(); // ≈ 1.5492
        assert!(
            (measured - expected).abs() / expected < 0.02,
            "measured RMS {measured} deviates >2% from expected {expected}"
        );
    }

    /// A 6 dB drop in target SNR must double the injected noise RMS,
    /// independent of the RNG implementation.
    #[test]
    fn awgn_2500hz_ref_scales_6db_as_factor_two() {
        // Same seed both sides, so the underlying standard-normal draws are
        // shared and only the σ scaling differs — the ratio is exact up to
        // f32 rounding. The 3% tolerance is slack, not a fitted constant.
        let n = 120_000;
        let mut a = vec![0.0_f32; n];
        let mut b = vec![0.0_f32; n];
        add_awgn_2500hz_ref(&mut a, 1.0, 0.0, 7);
        add_awgn_2500hz_ref(&mut b, 1.0, -6.0, 7);
        let ratio = signal_rms(&b) / signal_rms(&a);
        assert!(
            (ratio - 2.0).abs() < 0.06,
            "6 dB SNR drop gave RMS ratio {ratio}, expected ~2.0"
        );
    }

    /// AWGN must be zero-mean; a biased generator would shift the DC term.
    #[test]
    fn awgn_2500hz_ref_is_zero_mean() {
        // Std err of the mean is σ/sqrt(n) = 1.549/sqrt(120_000) ≈ 0.0045,
        // i.e. 0.29% of the RMS. The 2%-of-RMS bound is ~7σ.
        let n = 120_000;
        let mut buf = vec![0.0_f32; n];
        add_awgn_2500hz_ref(&mut buf, 1.0, 0.0, 99);
        let mean = buf.iter().map(|&s| s as f64).sum::<f64>() / n as f64;
        let rms = signal_rms(&buf);
        assert!(
            mean.abs() < 0.02 * rms,
            "mean {mean} is not negligible against RMS {rms}"
        );
    }

    /// The documented per-seed determinism contract (this module's header),
    /// asserted within a single build. Survives the bump; does not pin the
    /// stream — cross-version stream stability is explicitly not guaranteed
    /// (see `pancetta-research/README.md`).
    #[test]
    fn awgn_2500hz_ref_is_deterministic_per_seed() {
        let n = 4_096;
        let (mut a, mut b, mut c) = (vec![0.0_f32; n], vec![0.0_f32; n], vec![0.0_f32; n]);
        add_awgn_2500hz_ref(&mut a, 1.0, 3.0, 2026);
        add_awgn_2500hz_ref(&mut b, 1.0, 3.0, 2026);
        add_awgn_2500hz_ref(&mut c, 1.0, 3.0, 2027);
        assert_eq!(a, b, "same seed must reproduce the same noise");
        assert_ne!(a, c, "different seed must produce different noise");
    }
}
