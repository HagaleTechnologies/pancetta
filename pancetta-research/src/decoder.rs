use crate::Mode;
use serde::Serialize;
use std::path::Path;

/// One decoded message from a single WAV. Mode-agnostic.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Decode {
    /// Message string as the decoder produced it (e.g. "CQ K1ABC FN42").
    pub message: String,
    /// Audio frequency offset in Hz.
    pub freq_hz: f64,
    /// Time offset relative to slot start in seconds (DT).
    pub dt_s: f64,
    /// Decoder-reported SNR in dB (sign convention varies by mode; use raw).
    pub snr_db: f64,
    /// True if the CRC checked out. Pancetta returns only CRC-valid decodes
    /// today, so this is `true` for our impl; the field exists for parity
    /// with baseline tools that may report uncertain decodes.
    pub crc_valid: bool,
}

/// Generic interface for any decoder we want to evaluate. Implementors wrap
/// the production decoder, a baseline (jt9/JTDX), or an experimental variant.
pub trait DecoderUnderTest: Send + Sync {
    /// Mode this decoder targets.
    fn mode(&self) -> Mode;
    /// Stable identifier for this decoder (e.g. "pancetta-ft8@HEAD", "jt9").
    fn identity(&self) -> String;
    /// Decode a single WAV file. Errors should be returned as `Err`, not
    /// silently turned into empty decodes — the harness logs them.
    fn decode_wav(&self, path: &Path) -> anyhow::Result<Vec<Decode>>;
    /// Opaque JSON snapshot of effective config — serialized into the
    /// scorecard for reproducibility.
    fn config_snapshot(&self) -> serde_json::Value;
}
