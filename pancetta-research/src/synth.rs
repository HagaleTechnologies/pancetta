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
