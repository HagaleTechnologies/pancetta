use crate::Mode;
use anyhow::Context;
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

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

/// Wraps the production pancetta-ft8 decoder for use by the harness.
///
/// Holds an `Ft8Config` (the public config struct) and constructs a fresh
/// `pancetta_ft8::Ft8Decoder` per call to `decode_wav`. The production
/// decoder takes `&mut self` and we want this trait impl to be `Send + Sync`,
/// so we don't keep the decoder around between calls — construction is cheap.
pub struct Ft8Decoder {
    config: pancetta_ft8::Ft8Config,
    /// Used only so `config_snapshot` is stable across calls. Empty by
    /// default; future plans may stash per-experiment overrides here.
    _scratch: Mutex<()>,
}

impl Ft8Decoder {
    /// Build with default pancetta-ft8 config (matches what production uses
    /// on `main`).
    pub fn with_default_config() -> Self {
        Self {
            config: pancetta_ft8::Ft8Config::default(),
            _scratch: Mutex::new(()),
        }
    }

    /// Override `max_decode_passes` on the wrapped config. Used by the
    /// hb-001 sweep and any future experiments that want to vary this
    /// without touching the production default.
    pub fn with_max_passes(mut self, n: usize) -> Self {
        self.config.max_decode_passes = n;
        self
    }

    /// Override `max_sync_candidates` on the wrapped config (the
    /// Costas-search cap before NMS). hb-003 sweep.
    pub fn with_max_sync_candidates(mut self, n: usize) -> Self {
        self.config.max_sync_candidates = n;
        self
    }

    /// Override `max_candidates` on the wrapped config (the decode-side
    /// cap, post-NMS). Companion to `with_max_sync_candidates` for
    /// hb-003 sub-experiment (b).
    pub fn with_max_candidates(mut self, n: usize) -> Self {
        self.config.max_candidates = n;
        self
    }

    /// Override `osd_depth` on the wrapped config. `None` disables OSD
    /// entirely; `Some(0..=3)` selects depth. hb-005 sweep.
    pub fn with_osd_depth(mut self, depth: Option<u8>) -> Self {
        self.config.osd_depth = depth;
        self
    }

    /// Override `ldpc_iterations` on the wrapped config (BP iteration
    /// cap before OSD fallback). hb-005 sweep.
    pub fn with_ldpc_iterations(mut self, n: usize) -> Self {
        self.config.ldpc_iterations = n;
        self
    }

    /// Override `llr_target_variance` on the wrapped config. hb-006 sweep.
    pub fn with_llr_target_variance(mut self, v: f32) -> Self {
        self.config.llr_target_variance = v;
        self
    }

    /// Override `nms_enabled` on the wrapped config. hb-019 audit —
    /// disable to let all Costas peaks compete in LDPC.
    pub fn with_nms_enabled(mut self, enabled: bool) -> Self {
        self.config.nms_enabled = enabled;
        self
    }
}

impl DecoderUnderTest for Ft8Decoder {
    fn mode(&self) -> Mode {
        Mode::Ft8
    }

    fn identity(&self) -> String {
        format!("pancetta-ft8@{}", env!("CARGO_PKG_VERSION"))
    }

    fn decode_wav(&self, path: &Path) -> anyhow::Result<Vec<Decode>> {
        // Load WAV via hound; pancetta-ft8 expects mono f32 samples at 12 kHz
        // (FT8's canonical decode rate). The fixture and recording WAVs are
        // already at 12 kHz mono; assert and bail if not.
        let mut reader = hound::WavReader::open(path)
            .with_context(|| format!("opening WAV {}", path.display()))?;
        let spec = reader.spec();
        anyhow::ensure!(
            spec.channels == 1 && spec.sample_rate == 12000,
            "WAV {} not 12kHz mono (got {} ch, {} Hz)",
            path.display(),
            spec.channels,
            spec.sample_rate,
        );
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Int => reader
                .samples::<i16>()
                .map(|s| s.map(|v| v as f32 / 32768.0))
                .collect::<Result<Vec<_>, _>>()?,
            hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
        };

        // Construct a fresh decoder per WAV. decode_window takes &mut self,
        // and we want the outer trait impl to stay `&self`.
        let mut decoder = pancetta_ft8::Ft8Decoder::new(self.config.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new failed: {e}"))?;
        let raw = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window failed for {}: {e}", path.display()))?;
        Ok(raw
            .into_iter()
            .map(|d| Decode {
                message: d.text.clone(),
                freq_hz: d.frequency_offset,
                dt_s: d.time_offset,
                snr_db: d.snr_db as f64,
                crc_valid: true, // pancetta returns CRC-valid only
            })
            .collect())
    }

    fn config_snapshot(&self) -> serde_json::Value {
        // Prefer JSON-serialize; fall back to Debug-print if Ft8Config doesn't
        // (yet) derive Serialize.
        match serde_json::to_value(&self.config) {
            Ok(v) => v,
            Err(_) => serde_json::json!({
                "debug_repr": format!("{:?}", self.config),
            }),
        }
    }
}
