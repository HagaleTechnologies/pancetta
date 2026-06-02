use crate::Mode;
use anyhow::Context;
use serde::Serialize;
use std::path::{Path, PathBuf};
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
    /// hb-129: presentation-time-into-window (seconds elapsed from window
    /// start until this decode passed CRC). `None` for decoders that don't
    /// emit timing (jt9 subprocess, ft8_lib FFI). Used by the TTFD metric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_time_into_window_s: Option<f64>,
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
    /// Chronological-replay diagnostic (2026-06-01): when stateful mode
    /// is active, returns the count of accumulated callsigns in the
    /// cross-WAV snapshot. The default returns `None` for stateless or
    /// unsupported decoders (jt9, default-config Ft8Decoder); the
    /// chrono-replay tier uses the per-slot delta to confirm
    /// statefulness ("snapshot grows monotonically across consecutive
    /// WAVs").
    fn chrono_replay_snapshot_len(&self) -> Option<usize> {
        None
    }
}

/// Wraps the production pancetta-ft8 decoder for use by the harness.
///
/// Holds an `Ft8Config` (the public config struct) and constructs a fresh
/// `pancetta_ft8::Ft8Decoder` per call to `decode_wav`. The production
/// decoder takes `&mut self` and we want this trait impl to be `Send + Sync`,
/// so we don't keep the decoder around between calls — construction is cheap.
pub struct Ft8Decoder {
    config: pancetta_ft8::Ft8Config,
    /// Optional AP context to pass into `decode_window_with_ap`. When `None`,
    /// the decoder uses `decode_window` (which constructs a default-empty
    /// ApContext internally → ap_active=false → AP code paths short-circuit).
    /// hb-004 wiring: when set, AP fires in eval. Mutually exclusive with
    /// `rolling_window` — if both are set, rolling_window takes precedence.
    ap_context: Option<pancetta_ft8::ap::ApContext>,
    /// hb-050: when Some(N), maintains a rolling deque of the last N decoded
    /// callsigns across decode_wav calls. Each call builds an ApContext from
    /// the current deque contents (no my_call) and uses
    /// `decode_window_with_ap` so the hb-043 my_call-less AP path fires.
    /// After decoding, callsigns from new decodes are pushed into the deque.
    rolling_window: Option<usize>,
    rolling_calls: Mutex<std::collections::VecDeque<String>>,
    /// hb-046: when Some, run a "cheap" first pass with this config before
    /// the standard pass; union results dedup'd by message text. NOT the
    /// same as max_decode_passes (which is subtract-and-retry, shelved).
    /// Mutually exclusive with rolling_window and ap_context for now.
    two_stage_first_config: Option<pancetta_ft8::Ft8Config>,
    /// hb-057 V1 (Session 2): shared per-callsign DT history persisting
    /// across `decode_wav` calls. The eval harness creates one decoder
    /// wrapper and reuses it across all WAVs in a tier, so this is the
    /// natural place to hold the cross-WAV history (mirroring how
    /// pancetta-coordinator-scoped `CrossTimeState` would behave in
    /// production). `None` disables — `Ft8Config::dt_history_enabled`
    /// also gates the decoder-side narrowing.
    dt_history: Option<std::sync::Arc<pancetta_ft8::InMemoryDtHistory>>,
    /// Chronological-replay tier substrate (2026-06-01). When `Some`, this
    /// decoder is operating in stateful mode: each successful `decode_wav`
    /// pushes its decoded callsigns into the shared deque, and the NEXT
    /// `decode_wav` builds an `ApContext.recent_calls` from the current
    /// deque contents (no my_call — hb-043 my_call-less injection path).
    ///
    /// Distinct from `rolling_window` only in INTENT and labeling — both
    /// implement the same accumulator semantics on top of the AP
    /// `recent_calls` channel. The split exists so a future iter that
    /// wires `pancetta_qso::CrossTimeState` into pancetta-ft8 can replace
    /// THIS field with a `CrossTimeState` handle WITHOUT touching the
    /// rolling-window paths used by hb-050.
    chrono_replay_state: Option<ChronoReplayState>,
    /// Used only so `config_snapshot` is stable across calls.
    _scratch: Mutex<()>,
}

/// Per-decoder accumulated state for chronological replay. Cloned on
/// builder calls; the inner `Arc<Mutex<…>>` is shared so all clones see
/// the same growing snapshot.
#[derive(Clone, Default)]
pub struct ChronoReplayState {
    /// Capacity cap on the rolling deque. `0` = unbounded.
    pub capacity: usize,
    /// FIFO of bare callsigns seen across decode_wav calls.
    pub calls: std::sync::Arc<Mutex<std::collections::VecDeque<String>>>,
}

impl Ft8Decoder {
    /// Build with default pancetta-ft8 config (matches what production uses
    /// on `main`).
    pub fn with_default_config() -> Self {
        Self {
            config: pancetta_ft8::Ft8Config::default(),
            ap_context: None,
            rolling_window: None,
            rolling_calls: Mutex::new(std::collections::VecDeque::new()),
            two_stage_first_config: None,
            dt_history: None,
            chrono_replay_state: None,
            _scratch: Mutex::new(()),
        }
    }

    /// Chronological-replay tier (2026-06-01): enable stateful mode where
    /// callsigns decoded from one WAV are carried into the next WAV's
    /// `ApContext.recent_calls`. `capacity` caps the deque (use 0 for
    /// unbounded — the chrono-replay tier defaults to 0 since session
    /// length is what bounds growth in practice).
    ///
    /// Returns a NEW handle to `ChronoReplayState` for the caller to
    /// inspect mid-tier (e.g. smoke-test assertions that the snapshot
    /// grows monotonically). The decoder owns its own clone of the same
    /// `Arc`.
    pub fn with_chrono_replay(mut self, capacity: usize) -> (Self, ChronoReplayState) {
        let state = ChronoReplayState {
            capacity,
            calls: std::sync::Arc::new(Mutex::new(std::collections::VecDeque::new())),
        };
        self.chrono_replay_state = Some(state.clone());
        (self, state)
    }

    /// Read the current accumulated callsign snapshot. Returns an empty
    /// `Vec` when chrono-replay is disabled. Used by the eval harness for
    /// per-WAV diagnostics ("snapshot.len() = N going into slot K").
    pub fn chrono_replay_snapshot(&self) -> Vec<String> {
        match &self.chrono_replay_state {
            Some(s) => s
                .calls
                .lock()
                .map(|g| g.iter().cloned().collect())
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// hb-057 V1 (Session 2): enable the median-DT-per-callsign prior on
    /// the residual sync pass. Creates a shared `InMemoryDtHistory`
    /// persisting across `decode_wav` calls (the eval harness reuses one
    /// `Ft8Decoder` wrapper across all WAVs in a tier; the history
    /// mirrors what a coordinator-scoped `CrossTimeState` would carry in
    /// production).
    ///
    /// `floor_s` is the minimum prior-gate radius (default 0.2 per
    /// diagnostic); `iqr_scale` widens the gate proportional to IQR
    /// (default 3.0). Both forwarded to `Ft8Config`.
    pub fn with_dt_history(mut self, floor_s: f64, iqr_scale: f64) -> Self {
        self.config.dt_history_enabled = true;
        self.config.dt_history_window_floor_s = floor_s;
        self.config.dt_history_window_iqr_scale = iqr_scale;
        self.dt_history = Some(std::sync::Arc::new(
            pancetta_ft8::InMemoryDtHistory::default(),
        ));
        self
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

    /// Override `nms_time_radius` on the wrapped config. hb-008 sweep.
    pub fn with_nms_time_radius(mut self, n: usize) -> Self {
        self.config.nms_time_radius = n;
        self
    }

    /// Override `nms_freq_radius` on the wrapped config. hb-008 sweep.
    pub fn with_nms_freq_radius(mut self, n: usize) -> Self {
        self.config.nms_freq_radius = n;
        self
    }

    /// Override `nms_score_delta_db` on the wrapped config. hb-036:
    /// score-relative NMS suppression. Setting a non-zero value enables
    /// the score-relative gate that keeps "distinct weaker" signals while
    /// still suppressing near-duplicates of a strong signal. 0.0 is the
    /// legacy pure TF-distance NMS behavior.
    pub fn with_nms_score_delta_db(mut self, v: f64) -> Self {
        self.config.nms_score_delta_db = v;
        self
    }

    /// Override `min_sync_score` on the wrapped config. hb-007 sweep.
    pub fn with_min_sync_score(mut self, v: f64) -> Self {
        self.config.min_sync_score = v;
        self
    }

    /// Enable per-candidate adaptive LDPC iteration scheduling. hb-022 wild card.
    pub fn with_adaptive_ldpc_iters(mut self, on: bool) -> Self {
        self.config.adaptive_ldpc_iters = on;
        self
    }

    /// Override `time_range` (seconds of ± slot-time search). hb-025 audit.
    pub fn with_time_range(mut self, v: f64) -> Self {
        self.config.time_range = v;
        self
    }

    /// Override the OSD parity gate. hb-014 sweep candidate.
    pub fn with_max_parity_errors_for_osd(mut self, n: usize) -> Self {
        self.config.max_parity_errors_for_osd = n;
        self
    }

    /// hb-044: enable parabolic refinement of the Costas time-bin peak.
    pub fn with_sync_time_interpolation(mut self, on: bool) -> Self {
        self.config.sync_time_interpolation = on;
        self
    }

    /// hb-068 variant (a): only apply the parabolic refinement when the
    /// integer-bin sync score exceeds this threshold. 0.0 = no gate.
    pub fn with_sync_time_interp_score_gate(mut self, v: f64) -> Self {
        self.config.sync_time_interp_score_gate = v;
        self
    }

    /// hb-068 variant (b): multiply the parabolic delta by this factor
    /// before applying it. Score is recomputed from the parabola at the
    /// scaled offset. 1.0 = no scaling.
    pub fn with_sync_time_interp_delta_scale(mut self, v: f64) -> Self {
        self.config.sync_time_interp_delta_scale = v;
        self
    }

    /// hb-068 variant (c): reject parabolic refinements whose magnitude
    /// exceeds this threshold (fall back to integer-bin + original score).
    /// `None` disables rejection.
    pub fn with_sync_time_interp_max_delta_abs(mut self, v: Option<f64>) -> Self {
        self.config.sync_time_interp_max_delta_abs = v;
        self
    }

    /// hb-069: interpolate spectrogram lookups in linear power instead of
    /// dB. When true, `lookup_time_interp` converts each endpoint dB→
    /// linear, interpolates, then converts back to dB. Preserves symbol
    /// energy more accurately near the noise floor at the cost of two
    /// pow/log per call.
    pub fn with_sync_time_interp_linear_power(mut self, on: bool) -> Self {
        self.config.sync_time_interp_linear_power = on;
        self
    }

    /// hb-067: mBP offset — subtract this magnitude from each LLR
    /// before invoking OSD. 0.0 = no offset.
    pub fn with_bp_offset_subtract(mut self, v: f32) -> Self {
        self.config.bp_offset_subtract = v;
        self
    }

    /// hb-063: use a layered (row-sequential) BP schedule instead of
    /// the default flooding schedule.
    pub fn with_layered_bp(mut self, on: bool) -> Self {
        self.config.layered_bp = on;
        self
    }

    /// hb-056: enable cross-cycle non-coherent symbol averaging.
    pub fn with_cross_cycle_averaging(mut self, on: bool) -> Self {
        self.config.cross_cycle_averaging = on;
        self
    }

    /// hb-074: when paired with cross-cycle averaging, use the coherent
    /// (complex-spectrogram, phase-aligned) variant instead of non-coherent.
    pub fn with_cross_cycle_coherent(mut self, on: bool) -> Self {
        self.config.cross_cycle_coherent = on;
        self
    }

    /// hb-075: when paired with coherent cross-cycle, use MRC-style
    /// magnitude-weighting (multiply by conj(acc) directly) instead of
    /// unweighted unit-rotor alignment.
    pub fn with_cross_cycle_coherent_mrc(mut self, on: bool) -> Self {
        self.config.cross_cycle_coherent_mrc = on;
        self
    }

    /// hb-079 + hb-080: set the number of coherent subtract+repass rounds.
    /// 0 disables; 1 = original hb-079 (one round). hb-080 sweeps {2..5}.
    pub fn with_coherent_multipass_iterations(mut self, n: u8) -> Self {
        self.config.coherent_multipass_iterations = n;
        self
    }

    /// hb-081: MRC-weighted coherent subtract threshold (0 disables).
    pub fn with_coherent_subtract_mrc_threshold(mut self, t: f64) -> Self {
        self.config.coherent_subtract_mrc_threshold = t;
        self
    }

    /// hb-082: minimum Costas sync_score for the residual pass (None reuses
    /// production min_sync_score).
    pub fn with_residual_min_sync_score(mut self, t: Option<f64>) -> Self {
        self.config.residual_min_sync_score = t;
        self
    }

    /// hb-086 V1: after the multipass subtract loop, force-retry every
    /// ORIGINAL sync candidate not at an already-subtracted position
    /// against the residual spectrogram. Targets interference-pair
    /// recovery where B's residual sync_score falls below the threshold
    /// even though its post-A-subtract LLRs are decodable.
    pub fn with_joint_pair_retry(mut self, on: bool) -> Self {
        self.config.joint_pair_retry = on;
        self
    }

    /// hb-086 V3: dB relaxation for the bin-targeted residual sync
    /// pass. 0.0 disables; negative values lower `min_sync_score` by
    /// that magnitude only at freq_bins within
    /// `joint_residual_sync_window_bins` of subtracted decodes.
    pub fn with_joint_residual_sync_relax_db(mut self, db: f64) -> Self {
        self.config.joint_residual_sync_relax_db = db;
        self
    }

    /// hb-086 V3: half-width (in freq_bins) of the bin-targeting window
    /// for the V3 localized residual sync pass.
    pub fn with_joint_residual_sync_window_bins(mut self, n: usize) -> Self {
        self.config.joint_residual_sync_window_bins = n;
        self
    }

    /// hb-016: residual-energy early-stop margin (in dB above noise
    /// floor) for the coherent multipass loop. `None` disables; `Some(x)`
    /// makes each round bail when post-subtract residual mean dB is
    /// within `x` dB of the original spectrogram's median dB.
    pub fn with_residual_energy_stop_db(mut self, t: Option<f64>) -> Self {
        self.config.residual_energy_stop_db = t;
        self
    }

    /// hb-093: per-position residual SNR pre-decode gate (dB, WAV-relative).
    /// `None` disables; `Some(db)` skips LDPC in the joint_pair_retry path
    /// at any candidate whose residual SNR is below `db`.
    pub fn with_residual_snr_gate_db(mut self, t: Option<f64>) -> Self {
        self.config.residual_snr_gate_db = t;
        self
    }

    /// hb-093: enable diagnostic capture of per-candidate
    /// (sync_score, residual_snr_db, decoded_ok) records in the
    /// joint_pair_retry pass. Read out via
    /// `Ft8Decoder::take_residual_snr_diagnostic` on the underlying
    /// pancetta_ft8 decoder. Default false.
    pub fn with_residual_snr_diagnostic(mut self, on: bool) -> Self {
        self.config.residual_snr_diagnostic = on;
        self
    }

    /// hb-048 Session 3: enable the a7 template cross-correlation pass.
    /// Default false (off in production until graduation).
    pub fn with_a7_enabled(mut self, on: bool) -> Self {
        self.config.a7_enabled = on;
        self
    }

    /// hb-048: override the a7 snr7 acceptance threshold (default 6.0 per
    /// WSJT-X reference).
    pub fn with_a7_snr7_threshold(mut self, t: f64) -> Self {
        self.config.a7_snr7_threshold = t;
        self.config.a7_enabled = true;
        self
    }

    /// hb-048: override the a7 snr7b acceptance threshold (default 1.8 per
    /// WSJT-X reference).
    pub fn with_a7_snr7b_threshold(mut self, t: f64) -> Self {
        self.config.a7_snr7b_threshold = t;
        self.config.a7_enabled = true;
        self
    }

    /// hb-048: override the a7 freq-window (Hz) used to select
    /// sync_candidates around each expected call. Default 6.25 Hz (one
    /// pancetta freq_bin).
    pub fn with_a7_freq_window_hz(mut self, hz: f64) -> Self {
        self.config.a7_freq_window_hz = hz;
        self.config.a7_enabled = true;
        self
    }

    /// hb-046: enable two-stage decoding. When `on`, decode_wav runs a
    /// CHEAP pass first (relaxed sync_cap, no OSD, fewer LDPC iters)
    /// then the standard PRODUCTION pass on the same audio, unioning
    /// the decoded messages dedup'd by text. Distinct from
    /// max_decode_passes (which is subtract-and-retry, shelved).
    pub fn with_two_stage(mut self, on: bool) -> Self {
        if on {
            // Make the first pass MEANINGFULLY DIFFERENT, not just weaker:
            // - NMS ON (production has it OFF per hb-019); admits a different
            //   candidate population (merges adjacent peaks that production
            //   keeps separate).
            // - Slightly higher min_sync_score to compensate for the merge,
            //   keeping cost bounded.
            // This way the cheap pass can catch decodes the standard pass
            // missed due to candidate displacement.
            let mut cheap = self.config.clone();
            cheap.nms_enabled = true;
            cheap.max_sync_candidates = 200;
            self.two_stage_first_config = Some(cheap);
        } else {
            self.two_stage_first_config = None;
        }
        self
    }

    /// Attach an AP context. When `Some`, the decoder calls
    /// `decode_window_with_ap` per WAV — the AP1/AP2/AP3/AP4 code paths
    /// can fire if `ctx.my_call.is_some() || ctx.active_qso.is_some()`.
    /// hb-004 wiring (eval-AP infrastructure). Note: with the current
    /// pancetta-ft8 AP design, recent_calls injection requires my_call
    /// to also be set. A my_call-less AP path is hb-043 territory.
    pub fn with_ap_context(mut self, ctx: pancetta_ft8::ap::ApContext) -> Self {
        self.ap_context = Some(ctx);
        self
    }

    /// hb-050: enable rolling-callsign-window mode with capacity N. Each
    /// decode_wav call builds an ApContext.recent_calls from the deque
    /// contents and uses the my_call-less AP injection path (hb-043).
    /// After decoding, callsigns from the new decodes are pushed into the
    /// deque, evicting oldest. N=0 disables the feature.
    pub fn with_rolling_window(mut self, n: usize) -> Self {
        if n > 0 {
            self.rolling_window = Some(n);
        }
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

        // hb-046: optional two-stage decoding. Run a cheap first pass,
        // then merge with the standard pass. Mutually exclusive with
        // rolling_window, chrono_replay, and ap_context (those win if
        // also set — they take the AP path).
        let mut prelim: Vec<pancetta_ft8::DecodedMessage> = Vec::new();
        if self.two_stage_first_config.is_some()
            && self.rolling_window.is_none()
            && self.ap_context.is_none()
            && self.chrono_replay_state.is_none()
        {
            let cheap_cfg = self.two_stage_first_config.clone().unwrap();
            let mut cheap_decoder = pancetta_ft8::Ft8Decoder::new(cheap_cfg)
                .map_err(|e| anyhow::anyhow!("Ft8Decoder::new (cheap pass) failed: {e}"))?;
            prelim = cheap_decoder.decode_window(&samples).unwrap_or_default();
        }

        // Construct a fresh decoder per WAV. decode_window takes &mut self,
        // and we want the outer trait impl to stay `&self`.
        let mut decoder = pancetta_ft8::Ft8Decoder::new(self.config.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new failed: {e}"))?;
        // hb-057 V1: attach the shared DT history (Arc cloned per-WAV;
        // the Arc<dyn DtPriorLookup> erases the concrete type so the
        // decoder doesn't need to know about `InMemoryDtHistory`).
        if let Some(ref h) = self.dt_history {
            decoder = decoder
                .with_dt_priors(h.clone() as std::sync::Arc<dyn pancetta_ft8::DtPriorLookup>);
        }
        let raw = if let Some(state) = self.chrono_replay_state.clone() {
            // Chronological-replay tier (2026-06-01): build ApContext.recent_calls
            // from the persistent cross-WAV snapshot. The snapshot grows
            // monotonically across consecutive `decode_wav` calls — that is
            // the central statefulness guarantee of this tier.
            let snapshot: Vec<String> = state
                .calls
                .lock()
                .map(|g| g.iter().cloned().collect())
                .unwrap_or_default();
            let recent: Vec<pancetta_ft8::ap::RecentCallAp> = snapshot
                .iter()
                .filter_map(|c| pancetta_ft8::ap::RecentCallAp::new(c, 0.0))
                .collect();
            let ctx = pancetta_ft8::ap::ApContext {
                my_call: None,
                recent_calls: recent,
                active_qso: None,
            };
            let r = decoder.decode_window_with_ap(&samples, &ctx).map_err(|e| {
                anyhow::anyhow!(
                    "decode_window_with_ap (chrono-replay) failed for {}: {e}",
                    path.display()
                )
            })?;
            // Push every from/to callsign into the persistent deque. We
            // dedup against current contents so the snapshot doesn't grow
            // with re-sightings (the SET semantics here are what a future
            // `pancetta_qso::CrossTimeState`-backed implementation will
            // mirror, with TTL-based eviction replacing the capacity cap).
            if let Ok(mut deque) = state.calls.lock() {
                for msg in &r {
                    for cs in [
                        msg.message.from_callsign.as_deref(),
                        msg.message.to_callsign.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        let bare = cs.split('/').next().unwrap_or(cs).to_string();
                        if !bare.is_empty() && !deque.iter().any(|c| c == &bare) {
                            deque.push_back(bare);
                            if state.capacity > 0 {
                                while deque.len() > state.capacity {
                                    deque.pop_front();
                                }
                            }
                        }
                    }
                }
            }
            r
        } else if let Some(window_n) = self.rolling_window {
            // hb-050: build ApContext from current rolling deque snapshot.
            let snapshot: Vec<String> = self
                .rolling_calls
                .lock()
                .map(|g| g.iter().cloned().collect())
                .unwrap_or_default();
            let recent: Vec<pancetta_ft8::ap::RecentCallAp> = snapshot
                .iter()
                .filter_map(|c| pancetta_ft8::ap::RecentCallAp::new(c, 0.0))
                .collect();
            let ctx = pancetta_ft8::ap::ApContext {
                my_call: None,
                recent_calls: recent,
                active_qso: None,
            };
            let r = decoder.decode_window_with_ap(&samples, &ctx).map_err(|e| {
                anyhow::anyhow!(
                    "decode_window_with_ap (rolling) failed for {}: {e}",
                    path.display()
                )
            })?;
            // Update the deque with callsigns from these decodes.
            if let Ok(mut deque) = self.rolling_calls.lock() {
                for msg in &r {
                    if let Some(call) = msg.message.from_callsign.as_deref() {
                        let bare = call.split('/').next().unwrap_or(call).to_string();
                        if !bare.is_empty() && !deque.iter().any(|c| c == &bare) {
                            deque.push_back(bare);
                            while deque.len() > window_n {
                                deque.pop_front();
                            }
                        }
                    }
                    if let Some(call) = msg.message.to_callsign.as_deref() {
                        let bare = call.split('/').next().unwrap_or(call).to_string();
                        if !bare.is_empty() && !deque.iter().any(|c| c == &bare) {
                            deque.push_back(bare);
                            while deque.len() > window_n {
                                deque.pop_front();
                            }
                        }
                    }
                }
            }
            r
        } else {
            match &self.ap_context {
                Some(ctx) => decoder.decode_window_with_ap(&samples, ctx).map_err(|e| {
                    anyhow::anyhow!("decode_window_with_ap failed for {}: {e}", path.display())
                })?,
                None => decoder.decode_window(&samples).map_err(|e| {
                    anyhow::anyhow!("decode_window failed for {}: {e}", path.display())
                })?,
            }
        };
        // hb-057 V1: record each decoded (callsign, DT) into the shared
        // history BEFORE consuming `raw`/`prelim`. The next `decode_wav`
        // call (next WAV in this tier) will see the accumulated history.
        if let Some(ref h) = self.dt_history {
            let now = std::time::SystemTime::now();
            for d in raw.iter().chain(prelim.iter()) {
                if let Some(ref call) = d.message.from_callsign {
                    let bare = call.split('/').next().unwrap_or(call);
                    if !bare.is_empty() {
                        h.record(bare, d.time_offset, now);
                    }
                }
            }
        }

        // hb-046: merge prelim (cheap pass) + raw (standard pass) decodes,
        // dedup'd by message text. Prelim contributes any messages the
        // standard pass missed.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out: Vec<Decode> = Vec::new();
        let mut push = |d: pancetta_ft8::DecodedMessage| {
            if seen.insert(d.text.clone()) {
                out.push(Decode {
                    message: d.text.clone(),
                    freq_hz: d.frequency_offset,
                    dt_s: d.time_offset,
                    snr_db: d.snr_db as f64,
                    crc_valid: true,
                    // hb-129: presentation-time elapsed from window start
                    // when this decode passed CRC. Used by TTFD metric.
                    decode_time_into_window_s: d.decode_time_into_window.map(|t| t.as_secs_f64()),
                });
            }
        };
        for d in raw {
            push(d);
        }
        for d in prelim {
            push(d);
        }
        Ok(out)
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

    fn chrono_replay_snapshot_len(&self) -> Option<usize> {
        self.chrono_replay_state
            .as_ref()
            .map(|s| s.calls.lock().map(|g| g.len()).unwrap_or(0))
    }
}

// ============================================================================
// jt9 (WSJT-X) subprocess wrapper
// ============================================================================

/// Wraps the WSJT-X `jt9` CLI as a subprocess for FT8 decoding. Used to
/// generate baseline truth and (with `Ft8Decoder`) to identify
/// pancetta-only vs pancetta∩jt9 decodes for FP-filter training (hb-024
/// follow-up) and ensemble (hb-028).
///
/// Defaults the executable path to the macOS WSJT-X bundle. Override via
/// `with_executable_path` for Linux installs or non-default locations.
///
/// **Slot-length input only.** jt9 expects exactly one 15s FT8 slot per
/// invocation. For multi-slot WAVs (e.g., hard-200/1000's operator
/// recordings), enable `with_slot_cut(true)` — the wrapper will chunk
/// the audio into 15s slices, run jt9 on each tempfile, and aggregate
/// the decodes with adjusted dt offsets.
pub struct Jt9Decoder {
    executable: PathBuf,
    slot_cut: bool,
}

impl Default for Jt9Decoder {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("/Applications/wsjtx.app/Contents/MacOS/jt9"),
            slot_cut: false,
        }
    }
}

impl Jt9Decoder {
    pub fn with_executable_path(mut self, path: PathBuf) -> Self {
        self.executable = path;
        self
    }

    /// Enable slot-cutting: chunks the input WAV into 15s slices and runs
    /// jt9 on each. Required for multi-slot operator recordings. Adds
    /// tempfile + subprocess overhead per slot.
    pub fn with_slot_cut(mut self, on: bool) -> Self {
        self.slot_cut = on;
        self
    }

    /// Run jt9 on a single slot-length WAV file and parse the output.
    fn decode_one_file(&self, path: &Path) -> anyhow::Result<Vec<Decode>> {
        use std::process::Command;
        let out = Command::new(&self.executable)
            .arg("-8")
            .arg(path)
            .output()
            .with_context(|| format!("spawning jt9 at {}", self.executable.display()))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut decodes = Vec::new();
        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('<') {
                continue;
            }
            let tilde = match line.find('~') {
                Some(i) => i,
                None => continue,
            };
            let (prefix, suffix) = line.split_at(tilde);
            let fields: Vec<&str> = prefix.split_whitespace().collect();
            if fields.len() < 4 {
                continue;
            }
            let snr_db: f64 = fields[1].parse().unwrap_or(0.0);
            let dt_s: f64 = fields[2].parse().unwrap_or(0.0);
            let freq_hz: f64 = fields[3].parse().unwrap_or(0.0);
            let message = suffix[1..].trim().to_string();
            if message.is_empty() {
                continue;
            }
            decodes.push(Decode {
                message,
                freq_hz,
                dt_s,
                snr_db,
                crc_valid: true,
                // hb-129: jt9 doesn't expose per-decode timing.
                decode_time_into_window_s: None,
            });
        }
        Ok(decodes)
    }
}

impl DecoderUnderTest for Jt9Decoder {
    fn mode(&self) -> Mode {
        Mode::Ft8
    }

    fn identity(&self) -> String {
        format!("jt9@subprocess({})", self.executable.display())
    }

    fn decode_wav(&self, path: &Path) -> anyhow::Result<Vec<Decode>> {
        if !self.slot_cut {
            return self.decode_one_file(path);
        }
        // Slot-cut mode: split the WAV into 15s slices, run jt9 on each
        // tempfile, aggregate decodes with adjusted dt_s offsets.
        const SLOT_SECONDS: usize = 15;
        const SAMPLES_PER_SLOT: usize = 12000 * SLOT_SECONDS;
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
        // Read as i16 always — jt9 expects PCM16 input. If the source is
        // float WAV, convert.
        let samples: Vec<i16> = match spec.sample_format {
            hound::SampleFormat::Int => reader.samples::<i16>().collect::<Result<Vec<_>, _>>()?,
            hound::SampleFormat::Float => reader
                .samples::<f32>()
                .map(|s| s.map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16))
                .collect::<Result<Vec<_>, _>>()?,
        };
        let out_spec = hound::WavSpec {
            channels: 1,
            sample_rate: 12000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut all_decodes = Vec::new();
        for (slot_idx, chunk) in samples.chunks(SAMPLES_PER_SLOT).enumerate() {
            // FT8 active duration is 12.64s. Skip chunks too short to contain
            // a full message (require at least 12.0s of audio).
            if chunk.len() < 12000 * 12 {
                continue;
            }
            let tmp = tempfile::Builder::new()
                .prefix("pancetta-jt9-slot-")
                .suffix(".wav")
                .tempfile()
                .with_context(|| "creating tempfile for slot-cut")?;
            {
                let mut w = hound::WavWriter::create(tmp.path(), out_spec)
                    .with_context(|| "creating tempfile WAV writer")?;
                // Pad to exactly SAMPLES_PER_SLOT with zeros so jt9 sees a
                // canonical 15-second slot.
                for &s in chunk {
                    w.write_sample(s)?;
                }
                for _ in chunk.len()..SAMPLES_PER_SLOT {
                    w.write_sample(0i16)?;
                }
                w.finalize()?;
            }
            let mut slot_decodes = self.decode_one_file(tmp.path())?;
            let slot_offset_s = (slot_idx * SLOT_SECONDS) as f64;
            for d in &mut slot_decodes {
                d.dt_s += slot_offset_s;
            }
            all_decodes.extend(slot_decodes);
        }
        Ok(all_decodes)
    }

    fn config_snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "decoder": "jt9-subprocess",
            "executable": self.executable.display().to_string(),
        })
    }
}

#[cfg(test)]
mod jt9_tests {
    use super::*;

    #[test]
    fn jt9_decoder_identity_format() {
        let d = Jt9Decoder::default();
        assert!(d.identity().starts_with("jt9@subprocess("));
    }

    #[test]
    fn jt9_decoder_with_custom_path() {
        let d = Jt9Decoder::default().with_executable_path(PathBuf::from("/usr/local/bin/jt9"));
        assert!(d.identity().contains("/usr/local/bin/jt9"));
    }
}
