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
