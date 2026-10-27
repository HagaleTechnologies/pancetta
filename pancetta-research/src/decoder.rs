use crate::Mode;
use anyhow::Context;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
    /// W2.1 (decoder-tp-sensitivity plan): signal-domain acceptance
    /// metric fields, flattened from `pancetta_ft8::DecodedMessage::acceptance`
    /// (see `pancetta_ft8::acceptance`). `None` for decoders/paths that
    /// didn't compute one — see that module's doc for exactly which paths
    /// populate it. Used by the W2.1 calibration harness
    /// (`bin/acceptance_calibration.rs`) to correlate acceptance evidence
    /// against jt9-verified ground truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_distance: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_errors: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coherence: Option<f32>,
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
    /// Deterministic timestamp for the next offline slot.
    fn set_slot_time(&self, _at: std::time::SystemTime) {}
    /// Timing-history activity, not callsign snapshot size.
    fn timing_diagnostics(&self) -> pancetta_ft8::dt_history::TimingDiagnostics {
        Default::default()
    }
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
    /// PAN-165: per-window timing of the most recent `decode_wav` call,
    /// cleared on read. The default returns `None`; the research
    /// `Ft8Decoder` records one only when built with
    /// [`Ft8Decoder::with_budget_anchor`].
    fn take_last_window_timing(&self) -> Option<WindowTiming> {
        None
    }
    /// PAN-110: the native and ft8_lib decode texts behind the most recent
    /// `decode_wav` call's merged result, cleared on read. The default
    /// returns `None`; the research `Ft8Decoder` records them only when
    /// built with [`Ft8Decoder::with_ft8lib_merge`].
    fn take_last_window_decoder_texts(&self) -> Option<DecoderTexts> {
        None
    }
}

/// PAN-110: each backend's own decode texts for one window, before
/// production's ft8_lib-first merge combined them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecoderTexts {
    /// Texts the native decoder produced.
    pub native: Vec<String>,
    /// Texts the ft8_lib FFI decoder produced.
    pub ft8lib: Vec<String>,
}

/// PAN-165: where a harness decode window's `DecodeBudget` deadline starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetAnchor {
    /// The deadline starts right before the native decode call. This is the
    /// timing model every pre-PAN-165 budgeted scorecard used.
    Call,
    /// The deadline starts at window receipt, then production's pre-native
    /// sequence runs under it before the native call (waterfall, optional
    /// scoped fast-path, ft8_lib), as `pancetta/src/coordinator/ft8.rs` does.
    Receipt,
}

impl BudgetAnchor {
    /// Lower-case name, as the eval `--budget-anchor` flag spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetAnchor::Call => "call",
            BudgetAnchor::Receipt => "receipt",
        }
    }
}

/// PAN-165: where one decode window's wall-clock time went, in milliseconds.
/// Pre-native fields are zero under [`BudgetAnchor::Call`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WindowTiming {
    /// Waterfall generation, including the f32→f64 sample conversion.
    pub waterfall_ms: f64,
    /// Scoped fast-path decode around the QSO partner (0 when not run).
    pub scoped_ms: f64,
    /// ft8_lib FFI decode.
    pub ft8lib_ms: f64,
    /// Window receipt to the start of the native call.
    pub pre_native_ms: f64,
    /// Time left on the budget when the native call started (`None` =
    /// unlimited budget).
    pub headroom_at_native_ms: Option<f64>,
    /// The native decode call.
    pub native_ms: f64,
    /// Window receipt (or call start) to the end of the native call.
    pub window_total_ms: f64,
    /// How far the native call ended past the deadline (0 when on time or
    /// unlimited).
    pub over_deadline_ms: f64,
    /// Multipass-loop iterations the native call ran
    /// (`DecodeBudgetReport::passes_run`).
    pub passes_run: u32,
    /// The native call's `DecodeBudgetReport::budget_exhausted`.
    pub budget_exhausted: bool,
}

impl WindowTiming {
    /// Timing of a call-anchored native decode that ran from `start` to `end`.
    fn call_anchored(
        start: Instant,
        end: Instant,
        budget: &pancetta_ft8::DecodeBudget,
        report: &pancetta_ft8::DecodeBudgetReport,
    ) -> Self {
        let native_ms = duration_ms(end.saturating_duration_since(start));
        Self {
            headroom_at_native_ms: headroom_ms(budget, start),
            native_ms,
            window_total_ms: native_ms,
            over_deadline_ms: over_deadline_ms(budget, end),
            passes_run: report.passes_run,
            budget_exhausted: report.budget_exhausted,
            ..Self::default()
        }
    }
}

/// PAN-165: half-width, in 6.25 Hz bins, of production's scoped fast-path
/// decode around the QSO partner. Duplicated from `SCOPED_HALF_WIDTH` in
/// `pancetta/src/coordinator/ft8.rs`, because the harness cannot depend on
/// the `pancetta` binary crate. KEEP IN SYNC.
const SCOPED_HALF_WIDTH: usize = 5;

/// PAN-165: half-window (Hz) production narrows the native decode to around
/// the QSO partner. Duplicated from `DEFAULT_HALF_WINDOW_HZ` in
/// `pancetta/src/coordinator/qso_filter.rs`. KEEP IN SYNC.
const QSO_FILTER_DEFAULT_HALF_WINDOW_HZ: f64 = 60.0;

/// PAN-165: the native-call bin range production uses during a QSO. Same
/// formula as `partner_freq_to_bin_range` in
/// `pancetta/src/coordinator/qso_filter.rs`, at `TONE_SPACING` and the
/// default ±60 Hz half-window. `None` (full band) for a non-positive or
/// non-finite partner frequency, as there.
fn qso_filter_bin_range(partner_freq_hz: f64) -> Option<std::ops::RangeInclusive<usize>> {
    let spacing = pancetta_ft8::TONE_SPACING;
    if !partner_freq_hz.is_finite() || partner_freq_hz <= 0.0 {
        return None;
    }
    let center_bin = (partner_freq_hz / spacing).round() as i64;
    let half_bins = (QSO_FILTER_DEFAULT_HALF_WINDOW_HZ / spacing).ceil() as i64;
    let lo = center_bin.saturating_sub(half_bins).max(0) as usize;
    let hi = center_bin.saturating_add(half_bins).max(0) as usize;
    Some(lo..=hi)
}

fn duration_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Milliseconds left on `budget` at `at` (`None` = unlimited).
fn headroom_ms(budget: &pancetta_ft8::DecodeBudget, at: Instant) -> Option<f64> {
    budget
        .deadline()
        .map(|d| duration_ms(d.saturating_duration_since(at)))
}

/// Milliseconds past `budget`'s deadline at `at` (0 when on time or unlimited).
fn over_deadline_ms(budget: &pancetta_ft8::DecodeBudget, at: Instant) -> f64 {
    budget
        .deadline()
        .map_or(0.0, |d| duration_ms(at.saturating_duration_since(d)))
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
    ap_dt_history: Option<std::sync::Arc<pancetta_ft8::InMemoryDtHistory>>,
    observe_dt_history: bool,
    slot_time: Mutex<Option<std::time::SystemTime>>,
    timing_diagnostics: Mutex<pancetta_ft8::dt_history::TimingDiagnostics>,
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
    /// Task W3.3b: per-window wall-clock decode budget in milliseconds,
    /// mirroring production's `decode_effort_budget_ms` atomic
    /// (`pancetta/src/coordinator/effort.rs`). `None` (the default — no
    /// flag, no builder call) means every existing behavior is preserved
    /// byte-for-byte: `decode_budget_from()` maps `None` to
    /// `pancetta_ft8::DecodeBudget::unlimited()`, exactly what
    /// `decode_window`/`decode_window_with_ap` already used internally
    /// before this task. `Some(0)` (the "unlimited" preset name) is
    /// treated identically to `None` — both are the harness's "no
    /// deadline" sentinel, matching production's `decode_effort_budget_ms
    /// == 0` convention. `Some(ms)` with `ms > 0` builds a FRESH deadline
    /// (`Instant::now() + ms`) on every `decode_wav` call, since a budget
    /// is a per-window wall-clock deadline, not a fixed instant computed
    /// once at CLI-parse time (see `decode_budget_from`).
    budget_ms: Option<u64>,
    /// Task W5.4 [HARNESS]: fixed partner audio frequency (Hz) forwarded
    /// to every `decode_wav` call. `None` (default) reproduces the
    /// existing `partner_freq_hz = None` behavior byte-for-byte at every
    /// call site. See `with_partner_freq_hz`.
    partner_freq_hz: Option<f64>,
    /// PAN-165: timing model for the budget deadline. `None` (the default)
    /// records no timing and keeps every pre-existing behavior.
    /// `Some(Call)` records the native call's timing. `Some(Receipt)` runs
    /// production's pre-native sequence under a receipt-anchored deadline
    /// (see `decode_receipt_anchored`).
    budget_anchor: Option<BudgetAnchor>,
    /// PAN-165: simulated active-QSO partner frequency (Hz) for
    /// `BudgetAnchor::Receipt`: runs the scoped fast-path call and narrows
    /// the native call to the partner, as production does during a QSO.
    qso_partner_hz: Option<f64>,
    /// PAN-165: timing of the last `decode_wav`, for
    /// `take_last_window_timing`.
    last_window_timing: Mutex<Option<WindowTiming>>,
    /// PAN-110: under `BudgetAnchor::Receipt`, return production's merged
    /// decode set (ft8_lib first, then new native texts, then hash-twin
    /// dedup) instead of the native call's decodes alone.
    merge_ft8lib: bool,
    /// PAN-110: per-backend texts of the last merged `decode_wav`, for
    /// `take_last_window_decoder_texts`.
    last_window_decoder_texts: Mutex<Option<DecoderTexts>>,
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
            ap_dt_history: None,
            observe_dt_history: false,
            slot_time: Mutex::new(None),
            timing_diagnostics: Mutex::new(Default::default()),
            chrono_replay_state: None,
            budget_ms: None,
            partner_freq_hz: None,
            budget_anchor: None,
            qso_partner_hz: None,
            last_window_timing: Mutex::new(None),
            merge_ft8lib: false,
            last_window_decoder_texts: Mutex::new(None),
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

    /// hb-057 V2 (Session 3): override the frequency window (Hz) for
    /// per-candidate callsign-keyed sync narrowing. Default 25.0. Set
    /// to 0.0 to fall back to V1 (union-of-pass-1-callsigns) behavior.
    /// Maintain the legacy history and measure lookup eligibility in the
    /// control arm, without applying the V2 residual candidate filter.
    pub fn with_dt_history_control(mut self) -> Self {
        self = self.with_dt_history(0.2, 3.);
        self.config.dt_history_enabled = false;
        self.observe_dt_history = true;
        self
    }

    pub fn with_ap_dt_history(mut self, enabled: bool) -> Self {
        self.config.ap_dt_history_enabled = enabled;
        self.ap_dt_history = Some(std::sync::Arc::new(
            pancetta_ft8::InMemoryDtHistory::default(),
        ));
        self
    }

    /// Actual eligible sender priors, including in the control arm.
    pub fn eligible_timing_calls(
        &self,
        calls: &[String],
        at: std::time::SystemTime,
    ) -> Vec<String> {
        use pancetta_ft8::DtPriorLookup;
        calls
            .iter()
            .filter(|call| {
                self.ap_dt_history
                    .as_ref()
                    .and_then(|h| h.prior_at(call, at))
                    .is_some_and(|p| {
                        p.sighting_count >= 2
                            && p.iqr.is_finite()
                            && p.iqr >= 0.
                            && (3. * p.iqr).max(0.2) <= 0.6
                    })
            })
            .cloned()
            .collect()
    }

    pub fn with_dt_history_freq_window_hz(mut self, hz: f64) -> Self {
        self.config.dt_history_freq_window_hz = hz;
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

    /// Toggle learned ordering independently of OSD depth for attribution.
    pub fn with_neural_osd(mut self, enabled: bool) -> Self {
        self.config.neural_osd_enabled = enabled;
        self
    }

    /// Override `osd_input` on the wrapped config — Task W2.3 [A/B]:
    /// which LLR array drives OSD's search (BP-posterior vs. channel vs.
    /// offset-subtracted).
    pub fn with_osd_input(mut self, input: pancetta_ft8::OsdInput) -> Self {
        self.config.osd_input = input;
        self
    }

    /// Override `osd_npre2_preprocessing_enabled` on the wrapped config —
    /// Task W2.4 [A/B]: WSJT-X mainline-style npre2 warm-start
    /// preprocessing, active only at `osd_depth >= 3`.
    pub fn with_npre2_enabled(mut self, enabled: bool) -> Self {
        self.config.osd_npre2_preprocessing_enabled = enabled;
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

    /// Task W0.4 (2026-07-07): override the decoded protocol
    /// (`Ft8Config::protocol`). Default (from `Ft8Config::default()`) is
    /// `Protocol::Ft8`; the FT4 evaluation tier sets `Protocol::Ft4` so
    /// the wrapped `pancetta_ft8::Ft8Decoder` demodulates FT4 geometry
    /// (105 symbols, 4-tone, 7.5s cycle) instead of FT8's.
    pub fn with_protocol(mut self, protocol: pancetta_ft8::Protocol) -> Self {
        self.config.protocol = protocol;
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

    /// Task W3.5 [A/B]: combine each symbol's two TIME_OSR sub-steps in
    /// linear power instead of dB. Distinct from
    /// `with_sync_time_interp_linear_power` above (that one governs the
    /// SEPARATE fractional dt-shift lookup); this governs the
    /// unconditional two-substep average that runs on every symbol of
    /// every candidate.
    pub fn with_linear_power_averaging(mut self, on: bool) -> Self {
        self.config.linear_power_averaging = on;
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

    /// F1 [A/B]: use the Padé rational approximant for `atanh` in the
    /// LDPC sum-product check-node update instead of the exact ln form.
    /// See `Ft8Config::pade_atanh`.
    pub fn with_pade_atanh(mut self, on: bool) -> Self {
        self.config.pade_atanh = on;
        self
    }

    /// F5 [A/B]: disable the redundant half-symbol inner loop in the
    /// Costas sync kernel (Batch 92). See `Ft8Config::costas_half_loop_disabled`.
    pub fn with_costas_half_loop_disabled(mut self, on: bool) -> Self {
        self.config.costas_half_loop_disabled = on;
        self
    }

    /// Decoder-speed-overhaul Task 10 [A/B]: master switch for the BP
    /// escalation ladder. See `Ft8Config::escalation_enabled`.
    pub fn with_escalation_enabled(mut self, on: bool) -> Self {
        self.config.escalation_enabled = on;
        self
    }

    /// Decoder-TP-sensitivity Task W1.3 [A/B]: rectangular (no) window
    /// on the fine-FFT fallback's symbol-length FFT instead of Hann.
    /// See `Ft8Config::fine_fft_rect_window`.
    pub fn with_fine_fft_rect_window(mut self, on: bool) -> Self {
        self.config.fine_fft_rect_window = on;
        self
    }

    /// PAN-155 [A/B]: retry a failed DualMax FT8 AP0 candidate once with
    /// Bessel. See `Ft8Config::bessel_fallback_enabled`.
    pub fn with_bessel_fallback_enabled(mut self, on: bool) -> Self {
        self.config.bessel_fallback_enabled = on;
        self
    }

    /// Decoder-TP-sensitivity Task W3.3 [A/B]: master switch for the
    /// per-candidate fine-sync + matched-demod stage that replaces the
    /// legacy 21-trial fine-FFT fallback. See `Ft8Config::fine_sync_enabled`.
    pub fn with_fine_sync_enabled(mut self, on: bool) -> Self {
        self.config.fine_sync_enabled = on;
        self
    }

    /// hb-256 / PAN-162 [A/B]: impulse-robust per-symbol LLR weighting knee
    /// (`None` = off, the library default). See
    /// `Ft8Config::impulse_robust_llr`.
    pub fn with_impulse_robust_llr(mut self, knee: Option<f64>) -> Self {
        self.config.impulse_robust_llr = knee;
        self
    }

    /// Decoder-TP-sensitivity Task W3.4 [A/B]: master switch for the
    /// nsym=2/3 noncoherent combining LLR variants layered on top of the
    /// Task W3.3 matched-demod stage. Only takes effect when
    /// `fine_sync_enabled` is ALSO `true` — see
    /// `Ft8Config::nsym_combining_enabled`.
    pub fn with_nsym_combining_enabled(mut self, on: bool) -> Self {
        self.config.nsym_combining_enabled = on;
        self
    }

    /// Task W3.6 [A/B]: master switch for the per-candidate frequency
    /// tracker's re-test as a consumer of the Task W3.3 matched-demod
    /// stage (only takes effect when `fine_sync_enabled` is ALSO
    /// `true`). See `Ft8Config::per_candidate_freq_tracker_enabled`.
    pub fn with_per_candidate_freq_tracker_enabled(mut self, on: bool) -> Self {
        self.config.per_candidate_freq_tracker_enabled = on;
        self
    }

    /// Task W4.2 [A/B]: master switch for the per-block time-varying
    /// complex-amplitude GFSK subtraction path (replaces the legacy
    /// whole-signal single-estimate fit in `subtract_signal`). Only has
    /// any observable effect when `max_decode_passes >= 2` (the pass loop
    /// only calls `subtract_signal` between passes) — see
    /// `Ft8Config::time_varying_subtraction_enabled`. Added here in Task
    /// W4.3 because W4.2 never wired a CLI/harness flag (it was a proven
    /// dead path at the time); this task is the first that needs to
    /// actually drive it through `eval`.
    pub fn with_time_varying_subtraction_enabled(mut self, on: bool) -> Self {
        self.config.time_varying_subtraction_enabled = on;
        self
    }

    /// PAN-153 round-4 review finding: this flag had no harness/CLI
    /// wiring at all, unlike every neighboring coherent-subtraction
    /// knob — meaning the `compare` A/B workflow its own doc comment
    /// requires could never actually exercise it without hand-written
    /// code. See `Ft8Config::coherent_subtract_full_frame_reference_enabled`.
    pub fn with_coherent_subtract_full_frame_reference_enabled(mut self, on: bool) -> Self {
        self.config.coherent_subtract_full_frame_reference_enabled = on;
        self
    }

    /// PAN-168 [A/B]: per-block audio-domain cancellation on the
    /// full-frame coherent-subtraction path. Only takes effect when
    /// `coherent_subtract_full_frame_reference_enabled` is ALSO `true`. See
    /// `Ft8Config::coherent_subtract_full_frame_time_varying_enabled`.
    pub fn with_coherent_subtract_full_frame_time_varying_enabled(mut self, on: bool) -> Self {
        self.config
            .coherent_subtract_full_frame_time_varying_enabled = on;
        self
    }

    /// Task W4.2 [A/B]: independent sub-flag controlling whether the
    /// time-varying path subtracts its fitted estimate at full scale
    /// (1.0) vs. the legacy path's conservative 0.9 hold-back. Only takes
    /// effect when `time_varying_subtraction_enabled` is ALSO `true`. See
    /// `Ft8Config::full_scale_subtraction_enabled` and the W4.2 review's
    /// carry-forward warning: this sub-flag is UNTESTED on weak signals
    /// and should be A/B'd independently of the parent mechanism.
    pub fn with_full_scale_subtraction_enabled(mut self, on: bool) -> Self {
        self.config.full_scale_subtraction_enabled = on;
        self
    }

    /// Task W3.3b [HARNESS]: set the per-window wall-clock decode budget
    /// (milliseconds). `None` (the default) reproduces
    /// `DecodeBudget::unlimited()` exactly, matching every prior eval
    /// invocation. `Some(0)` is also treated as unlimited (matches
    /// production's `decode_effort_budget_ms == 0` sentinel). This lets
    /// the eval harness re-measure an A/B under a REAL bounded
    /// `DecodeBudget` (e.g. the `Standard`=250ms/`Eco`=1ms effort
    /// presets) instead of always using an unlimited budget — see
    /// `research/experiments/2026-07-08-w33-matched-demod-fine-sync.md`
    /// ("DecodeBudget integration") for why this matters: the existing
    /// S1-floor/S2-rest candidate split already gates per-candidate
    /// dispatch on `current_budget.has_time()`, so a real bounded budget
    /// should self-limit an expensive new stage's cost automatically —
    /// this builder is what makes that measurable.
    pub fn with_effort_budget_ms(mut self, ms: Option<u64>) -> Self {
        self.budget_ms = ms;
        self
    }

    /// PAN-165: choose the timing model for the budget deadline and start
    /// recording per-window timing (read it with `take_last_window_timing`).
    /// `BudgetAnchor::Receipt` works only on the plain decode path: no
    /// chrono replay, rolling window, two-stage decoding, or AP context
    /// beyond a `my_call` (PAN-110).
    pub fn with_budget_anchor(mut self, anchor: BudgetAnchor) -> Self {
        self.budget_anchor = Some(anchor);
        self
    }

    /// PAN-110: under `BudgetAnchor::Receipt`, score what production
    /// delivers rather than the native call alone: ft8_lib's decodes, then
    /// each native decode with new text
    /// ([`pancetta_ft8::merge::merge_ft8lib_first`]), then
    /// [`pancetta_ft8::merge::drop_unresolved_hash_twins`]. Each backend's
    /// own texts are kept for `take_last_window_decoder_texts`. Has no
    /// effect without the receipt anchor.
    pub fn with_ft8lib_merge(mut self, on: bool) -> Self {
        self.merge_ft8lib = on;
        self
    }

    /// PAN-165: simulate an active QSO with a partner at `hz` under
    /// `BudgetAnchor::Receipt`. Each window then runs production's scoped
    /// fast-path call around the partner, and the native call is narrowed
    /// to the partner ±60 Hz with `hz` as its relaxed-sync partner
    /// frequency (superseding `with_partner_freq_hz` on that call).
    pub fn with_qso_partner_hz(mut self, hz: f64) -> Self {
        self.qso_partner_hz = Some(hz);
        self
    }

    /// Build the `DecodeBudget` for a decode window whose deadline is
    /// anchored at `anchor`. A budget is a wall-clock DEADLINE, so this
    /// must be called freshly per WAV (not once at construction), with the
    /// `Instant` the window's timing starts from.
    fn decode_budget_from(&self, anchor: Instant) -> pancetta_ft8::DecodeBudget {
        match self.budget_ms {
            None | Some(0) => pancetta_ft8::DecodeBudget::unlimited(),
            Some(ms) => pancetta_ft8::DecodeBudget::until(anchor + Duration::from_millis(ms)),
        }
    }

    fn store_window_timing(&self, timing: Option<WindowTiming>) {
        *self
            .last_window_timing
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = timing;
    }

    fn store_window_decoder_texts(&self, texts: Option<DecoderTexts>) {
        *self
            .last_window_decoder_texts
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = texts;
    }

    /// PAN-110: the AP context a receipt-anchored decode may carry. Only a
    /// `my_call` is allowed: production's recent-call pool is empty at
    /// start-up and its active-QSO entries need a live QSO, so any other
    /// AP input would model a station state, not the shipped defaults.
    fn receipt_ap_context_is_my_call_only(&self) -> bool {
        self.ap_context.as_ref().is_none_or(|ctx| {
            ctx.recent_calls.is_empty() && ctx.active_qso.is_none() && ctx.active_qsos.is_empty()
        })
    }

    /// Run one native decode call under a budget anchored right before it
    /// (the harness's pre-PAN-165 timing model). Under
    /// `BudgetAnchor::Call` the call's `WindowTiming` is recorded.
    fn run_call_anchored<F>(
        &self,
        call: F,
    ) -> pancetta_ft8::Ft8Result<Vec<pancetta_ft8::DecodedMessage>>
    where
        F: FnOnce(
            pancetta_ft8::DecodeBudget,
        ) -> pancetta_ft8::Ft8Result<(
            Vec<pancetta_ft8::DecodedMessage>,
            pancetta_ft8::DecodeBudgetReport,
        )>,
    {
        let start = Instant::now();
        let budget = self.decode_budget_from(start);
        let (messages, report) = call(budget)?;
        if self.budget_anchor == Some(BudgetAnchor::Call) {
            self.store_window_timing(Some(WindowTiming::call_anchored(
                start,
                Instant::now(),
                &budget,
                &report,
            )));
        }
        Ok(messages)
    }

    /// PAN-165: reproduce production's per-window sequence from
    /// `pancetta/src/coordinator/ft8.rs` under one deadline anchored at
    /// window receipt: waterfall generation, the scoped fast-path call
    /// (only with a QSO partner), the ft8_lib FFI call, then the native
    /// call. Only the native call's decodes are returned (PAN-165
    /// Decision 6); the other calls run for their timing cost. With
    /// [`Self::with_ft8lib_merge`] (PAN-110) the ft8_lib decodes are kept
    /// and merged as production merges them, and both sides' texts are
    /// returned too.
    fn decode_receipt_anchored(
        &self,
        decoder: &mut pancetta_ft8::Ft8Decoder,
        samples: &[f32],
    ) -> anyhow::Result<(
        Vec<pancetta_ft8::DecodedMessage>,
        WindowTiming,
        Option<DecoderTexts>,
    )> {
        let anchor = Instant::now();
        let budget = self.decode_budget_from(anchor);

        let waterfall_start = Instant::now();
        let samples_f64: Vec<f64> = samples.iter().map(|&s| s as f64).collect();
        // Production logs and fans the waterfall out; a failure there only
        // logs a warning, so the result is not needed here either.
        let _ = decoder.generate_waterfall_data(&samples_f64);
        let waterfall_ms = duration_ms(waterfall_start.elapsed());

        let scoped_ms = match self.qso_partner_hz {
            Some(hz) => {
                let center = (hz / pancetta_ft8::TONE_SPACING).round() as usize;
                let lo = center.saturating_sub(SCOPED_HALF_WIDTH);
                let hi = center.saturating_add(SCOPED_HALF_WIDTH);
                let scoped_start = Instant::now();
                // Production discards a failed scoped call (`unwrap_or_default`).
                let _ = decoder.decode_window_with_ap_scoped_partner_budgeted(
                    samples,
                    &pancetta_ft8::ApContext::default(),
                    Some(lo..=hi),
                    None,
                    budget,
                );
                duration_ms(scoped_start.elapsed())
            }
            None => 0.0,
        };

        let ft8lib_start = Instant::now();
        let ft8lib_messages =
            pancetta_ft8::Ft8Decoder::decode_window_ft8lib_protocol(samples, self.config.protocol);
        let ft8lib_ms = duration_ms(ft8lib_start.elapsed());

        let (freq_bin_range, partner_freq_hz) = match self.qso_partner_hz {
            Some(hz) => (qso_filter_bin_range(hz), Some(hz)),
            None => (None, self.partner_freq_hz),
        };
        // PAN-110: production passes its AP context (my_call, recent calls,
        // active QSOs) to this call; `decode_wav` admits only a my_call here.
        let default_ap = pancetta_ft8::ApContext::default();
        let ap_context = self.ap_context.as_ref().unwrap_or(&default_ap);
        let native_start = Instant::now();
        let (native_messages, report) = decoder
            .decode_window_with_ap_scoped_partner_budgeted(
                samples,
                ap_context,
                freq_bin_range,
                partner_freq_hz,
                budget,
            )
            .map_err(|e| anyhow::anyhow!("receipt-anchored native decode failed: {e}"))?;
        let end = Instant::now();

        let timing = WindowTiming {
            waterfall_ms,
            scoped_ms,
            ft8lib_ms,
            pre_native_ms: duration_ms(native_start.saturating_duration_since(anchor)),
            headroom_at_native_ms: headroom_ms(&budget, native_start),
            native_ms: duration_ms(end.saturating_duration_since(native_start)),
            window_total_ms: duration_ms(end.saturating_duration_since(anchor)),
            over_deadline_ms: over_deadline_ms(&budget, end),
            passes_run: report.passes_run,
            budget_exhausted: report.budget_exhausted,
        };
        if !self.merge_ft8lib {
            return Ok((native_messages, timing, None));
        }
        let texts = DecoderTexts {
            native: native_messages.iter().map(|m| m.text.clone()).collect(),
            ft8lib: ft8lib_messages.iter().map(|m| m.text.clone()).collect(),
        };
        let merged = pancetta_ft8::merge::merge_ft8lib_first(ft8lib_messages, native_messages);
        let (delivered, _) =
            pancetta_ft8::merge::drop_unresolved_hash_twins(merged.messages, self.config.protocol);
        Ok((delivered, timing, Some(texts)))
    }

    /// Decoder-TP-sensitivity Task W1.4 [A/B]: master switch for the
    /// divisive LLR whitening step (per-tone/per-symbol noise-median
    /// normalisation). Default ON in production
    /// (`Ft8Config::llr_whitening_enabled`, graduated Batch 53); this
    /// hook lets the research harness re-measure with it forced off
    /// after the Task W1.4 dB/linear-magnitude unit-consistency fix,
    /// since the original "+4 TP / -713 FP" graduation measurement may
    /// not survive the fix. See `Ft8Config::llr_whitening_enabled`.
    pub fn with_llr_whitening(mut self, on: bool) -> Self {
        self.config.llr_whitening_enabled = on;
        self
    }

    /// Decoder-TP-sensitivity Task W2.5 [A/B]: master switch for the
    /// acceptance-metric-based post-CRC gate that replaces the blunt
    /// `MIN_DECODE_CONFIDENCE`/`MIN_AP_CONFIDENCE` sync-score floors. See
    /// `Ft8Config::acceptance_gating_enabled`.
    pub fn with_acceptance_gating_enabled(mut self, on: bool) -> Self {
        self.config.acceptance_gating_enabled = on;
        self
    }

    /// Decoder-TP-sensitivity Task W2.6 [A/B]: master switch for
    /// `ApLevel::Cq` (assume a failed-AP0 candidate is a plain "CQ"
    /// call). See `Ft8Config::cq_ap_enabled`.
    pub fn with_cq_ap_enabled(mut self, on: bool) -> Self {
        self.config.cq_ap_enabled = on;
        self
    }

    /// Decoder-TP-sensitivity Task W2.6 [A/B]: master switch for the AP4
    /// full message-content mask (RR73/RRR/73). See
    /// `Ft8Config::ap4_full_message_mask_enabled`.
    pub fn with_ap4_full_message_mask_enabled(mut self, on: bool) -> Self {
        self.config.ap4_full_message_mask_enabled = on;
        self
    }

    /// Decoder-TP-sensitivity Task W2.6 [A/B]: AP injection/normalization
    /// ordering. See `Ft8Config::ap_injection_post_normalization`.
    pub fn with_ap_injection_post_normalization(mut self, on: bool) -> Self {
        self.config.ap_injection_post_normalization = on;
        self
    }

    /// Decoder-speed-overhaul Task 9/10: shallow BP iteration count for
    /// the S1/S2 primary decode. See `Ft8Config::floor_iters`.
    pub fn with_floor_iters(mut self, n: usize) -> Self {
        self.config.floor_iters = n;
        self
    }

    /// Decoder-speed-overhaul Task 10: deep BP iteration count a
    /// near-miss floor failure is escalated to. See
    /// `Ft8Config::deep_iters`.
    pub fn with_deep_iters(mut self, n: usize) -> Self {
        self.config.deep_iters = n;
        self
    }

    /// Decoder-speed-overhaul Task 10: maximum unsatisfied parity
    /// checks tolerated at `floor_iters` before escalating. See
    /// `Ft8Config::escalation_parity_max`.
    pub fn with_escalation_parity_max(mut self, n: usize) -> Self {
        self.config.escalation_parity_max = n;
        self
    }

    /// Decoder-TP-sensitivity Task W5.1 [A/B]: master switch for per-bin
    /// peak candidate selection. See
    /// `Ft8Config::per_bin_candidate_selection`.
    pub fn with_per_bin_candidate_selection(mut self, on: bool) -> Self {
        self.config.per_bin_candidate_selection = on;
        self
    }

    /// PAN-7 [A/B]: seed the pass-0 sync-candidate list from ft8_lib's own
    /// `ftx_find_candidates` output. Default OFF; this builder plus the
    /// `--ft8lib-sync-seeds` / `--no-ft8lib-sync-seeds` eval flags are what make
    /// the ship gate measurable at all.
    pub fn with_ft8lib_sync_seeds_enabled(mut self, on: bool) -> Self {
        self.config.ft8lib_sync_seeds_enabled = on;
        self
    }

    /// Decoder-TP-sensitivity Task W5.2 [A/B]: master switch for the
    /// percentile-normalized wide-lag two-baseline sync mechanism. See
    /// `Ft8Config::costas_two_baseline_enabled`.
    pub fn with_costas_two_baseline_enabled(mut self, on: bool) -> Self {
        self.config.costas_two_baseline_enabled = on;
        self
    }

    /// Decoder-TP-sensitivity Task W5.4 [A/B]: master switch for the
    /// sync_bc partial-Costas (blocks B+C only) parallel score. See
    /// `Ft8Config::costas_partial_metric_enabled`.
    pub fn with_costas_partial_metric_enabled(mut self, on: bool) -> Self {
        self.config.costas_partial_metric_enabled = on;
        self
    }

    /// Decoder-TP-sensitivity Task W5.4 [A/B]: JTDX-style relaxed Costas
    /// acceptance threshold near the QSO partner audio frequency. Sets
    /// both the radius (Hz) and the score delta (dB, applied to
    /// `min_sync_score`). See `Ft8Config::relaxed_sync_near_partner_hz_radius`
    /// / `_score_delta`. Only has an observable effect when a
    /// `partner_freq_hz` is ALSO supplied per-call (`with_partner_freq_hz`).
    pub fn with_relaxed_sync_near_partner(mut self, radius_hz: f64, score_delta: f64) -> Self {
        self.config.relaxed_sync_near_partner_hz_radius = Some(radius_hz);
        self.config.relaxed_sync_near_partner_score_delta = score_delta;
        self
    }

    /// Decoder-TP-sensitivity Task W5.4 [HARNESS]: forward a fixed
    /// partner audio frequency (Hz) to every `decode_wav` call, simulating
    /// an active QSO parked at this frequency for the whole run. Threaded
    /// into `decode_window_with_ap_scoped_partner_budgeted` at every call
    /// site (constructing a default-empty `ApContext` when none was
    /// otherwise configured, so the partner-freq argument still reaches
    /// the decoder without accidentally enabling AP).
    pub fn with_partner_freq_hz(mut self, hz: f64) -> Self {
        self.partner_freq_hz = Some(hz);
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

    /// Task W4.4 [A/B] (decoder-tp-sensitivity plan): content guard for
    /// `group_for_cross_cycle` — require LLR-sign correlation between a
    /// candidate and its group's seed to be `>= threshold` before
    /// accepting it into the group. `None` (default) preserves the
    /// original geometric-only grouping.
    pub fn with_cross_cycle_content_guard(mut self, threshold: Option<f32>) -> Self {
        self.config.cross_cycle_content_guard = threshold;
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
        // Task W0.4: report the wrapped protocol rather than hardcoding
        // Ft8 — `with_protocol` (used by the FT4 evaluation tier) can
        // change `self.config.protocol` after construction.
        match self.config.protocol {
            pancetta_ft8::Protocol::Ft4 => Mode::Ft4,
            _ => Mode::Ft8,
        }
    }

    fn identity(&self) -> String {
        format!("pancetta-ft8@{}", env!("CARGO_PKG_VERSION"))
    }

    fn decode_wav(&self, path: &Path) -> anyhow::Result<Vec<Decode>> {
        // PAN-165: never let a failed call leave the previous window's
        // timing behind for `take_last_window_timing`.
        self.store_window_timing(None);
        self.store_window_decoder_texts(None);
        let receipt_anchored = self.budget_anchor == Some(BudgetAnchor::Receipt);
        anyhow::ensure!(
            !receipt_anchored
                || (self.chrono_replay_state.is_none()
                    && self.rolling_window.is_none()
                    && self.receipt_ap_context_is_my_call_only()
                    && self.two_stage_first_config.is_none()),
            "budget anchor `receipt` reproduces production's plain decode path and cannot be \
             combined with chrono replay, a rolling window, an AP context beyond my_call or \
             two-stage decoding"
        );
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
        let slot_time = self
            .slot_time
            .lock()
            .ok()
            .and_then(|g| *g)
            .unwrap_or_else(std::time::SystemTime::now);
        if self.dt_history.is_some() || self.ap_dt_history.is_some() {
            decoder = decoder
                .with_dt_priors(std::sync::Arc::new(TimingLookup {
                    legacy: self.dt_history.clone(),
                    fresh: self.ap_dt_history.clone(),
                }))
                .with_dt_prior_time(slot_time);
        }
        if self.observe_dt_history {
            decoder = decoder.with_dt_history_diagnostics();
        }
        let raw = if receipt_anchored {
            let (messages, timing, texts) = self
                .decode_receipt_anchored(&mut decoder, &samples)
                .with_context(|| format!("decoding {}", path.display()))?;
            self.store_window_timing(Some(timing));
            self.store_window_decoder_texts(texts);
            messages
        } else if let Some(state) = self.chrono_replay_state.clone() {
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
                active_qsos: Vec::new(),
            };
            let r = self
                .run_call_anchored(|budget| {
                    decoder.decode_window_with_ap_scoped_partner_budgeted(
                        &samples,
                        &ctx,
                        None,
                        self.partner_freq_hz,
                        budget,
                    )
                })
                .map_err(|e| {
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
                active_qsos: Vec::new(),
            };
            let r = self
                .run_call_anchored(|budget| {
                    decoder.decode_window_with_ap_scoped_partner_budgeted(
                        &samples,
                        &ctx,
                        None,
                        self.partner_freq_hz,
                        budget,
                    )
                })
                .map_err(|e| {
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
                Some(ctx) => self
                    .run_call_anchored(|budget| {
                        decoder.decode_window_with_ap_scoped_partner_budgeted(
                            &samples,
                            ctx,
                            None,
                            self.partner_freq_hz,
                            budget,
                        )
                    })
                    .map_err(|e| {
                        anyhow::anyhow!("decode_window_with_ap failed for {}: {e}", path.display())
                    })?,
                // Task W5.4 [HARNESS]: no ApContext was otherwise
                // configured, but a fixed partner_freq_hz was requested —
                // build a default-empty ApContext (AP stays inert: no
                // my_call, no recent_calls, no active_qso) purely so the
                // partner-frequency argument reaches the decoder.
                None if self.partner_freq_hz.is_some() => {
                    let ctx = pancetta_ft8::ap::ApContext {
                        my_call: None,
                        recent_calls: Vec::new(),
                        active_qso: None,
                        active_qsos: Vec::new(),
                    };
                    self.run_call_anchored(|budget| {
                        decoder.decode_window_with_ap_scoped_partner_budgeted(
                            &samples,
                            &ctx,
                            None,
                            self.partner_freq_hz,
                            budget,
                        )
                    })
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "decode_window_with_ap (partner-freq-only) failed for {}: {e}",
                            path.display()
                        )
                    })?
                }
                None => self
                    .run_call_anchored(|budget| decoder.decode_window_budgeted(&samples, budget))
                    .map_err(|e| {
                        anyhow::anyhow!("decode_window failed for {}: {e}", path.display())
                    })?,
            }
        };
        if let Ok(mut diagnostics) = self.timing_diagnostics.lock() {
            *diagnostics = decoder.timing_diagnostics();
        }
        if let Some(history) = &self.ap_dt_history {
            record_ap_timing_slot(history, raw.iter().chain(prelim.iter()), slot_time);
        }
        // hb-057 V2 (Session 3): record each decoded (callsign, DT, freq)
        // into the shared history BEFORE consuming `raw`/`prelim`. The
        // next `decode_wav` call (next WAV in this tier) sees the
        // accumulated history; the freq channel feeds the per-candidate
        // callsign-keyed sync narrowing in
        // `coherent_subtract_and_repass`.
        if let Some(ref h) = self.dt_history {
            let now = std::time::SystemTime::now();
            for d in raw.iter().chain(prelim.iter()) {
                if let Some(ref call) = d.message.from_callsign {
                    let bare = call.split('/').next().unwrap_or(call);
                    if !bare.is_empty() {
                        h.record_with_freq(bare, d.time_offset, d.frequency_offset, now);
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
                    // W2.1: flatten the acceptance metric (if computed) for
                    // the calibration harness.
                    soft_distance: d.acceptance.map(|a| a.soft_distance),
                    hard_errors: d.acceptance.map(|a| a.hard_errors),
                    coherence: d.acceptance.and_then(|a| a.coherence),
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

    fn set_slot_time(&self, at: std::time::SystemTime) {
        if let Ok(mut time) = self.slot_time.lock() {
            *time = Some(at);
        }
    }

    fn timing_diagnostics(&self) -> pancetta_ft8::dt_history::TimingDiagnostics {
        self.timing_diagnostics
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    fn config_snapshot(&self) -> serde_json::Value {
        // Prefer JSON-serialize; fall back to Debug-print if Ft8Config doesn't
        // (yet) derive Serialize.
        let mut snapshot = match serde_json::to_value(&self.config) {
            Ok(v) => v,
            Err(_) => serde_json::json!({
                "debug_repr": format!("{:?}", self.config),
            }),
        };
        // `neural_osd_enabled` alone can't distinguish which weight blob is
        // compiled in (see `pancetta_ft8::neural_osd_weights::provenance_sha256`
        // doc comment) — record it so A/B scorecards comparing two
        // `neural_osd_enabled: true` arms can prove they actually ran
        // different (or the same) model.
        if let serde_json::Value::Object(ref mut map) = snapshot {
            map.insert(
                "neural_osd_weights_sha256".to_string(),
                serde_json::Value::String(
                    pancetta_ft8::neural_osd_weights::provenance_sha256().to_string(),
                ),
            );
        }
        snapshot
    }

    fn chrono_replay_snapshot_len(&self) -> Option<usize> {
        self.chrono_replay_state
            .as_ref()
            .map(|s| s.calls.lock().map(|g| g.len()).unwrap_or(0))
    }

    fn take_last_window_timing(&self) -> Option<WindowTiming> {
        self.last_window_timing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn take_last_window_decoder_texts(&self) -> Option<DecoderTexts> {
        self.last_window_decoder_texts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
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
                // W2.1: jt9 is not the native decoder — no acceptance metric.
                soft_distance: None,
                hard_errors: None,
                coherence: None,
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

    #[test]
    fn neural_osd_attribution_toggle_reaches_production_config() {
        let enabled = Ft8Decoder::with_default_config().with_neural_osd(true);
        let disabled = Ft8Decoder::with_default_config().with_neural_osd(false);
        assert!(enabled.config.neural_osd_enabled);
        assert!(!disabled.config.neural_osd_enabled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bessel_fallback_snapshot_tracks_opt_in_without_changing_primary_metric() {
        let baseline = Ft8Decoder::with_default_config();
        let variant = Ft8Decoder::with_default_config().with_bessel_fallback_enabled(true);
        assert_eq!(baseline.config_snapshot()["bessel_fallback_enabled"], false);
        assert_eq!(variant.config_snapshot()["bessel_fallback_enabled"], true);
        assert_eq!(baseline.config_snapshot()["llr_metric"], "DualMax");
        assert_eq!(variant.config_snapshot()["llr_metric"], "DualMax");
    }

    fn fixture(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate dir has a parent")
            .join("pancetta-ft8/tests/fixtures/wav")
            .join(rel)
    }

    fn receipt_timing(decoder: &Ft8Decoder, rel: &str) -> WindowTiming {
        decoder
            .decode_wav(&fixture(rel))
            .expect("receipt-anchored decode should not error");
        decoder
            .take_last_window_timing()
            .expect("a receipt-anchored decode records its window timing")
    }

    #[test]
    fn no_anchor_records_no_timing_and_snapshot_is_unchanged() {
        let decoder = Ft8Decoder::with_default_config();
        decoder
            .decode_wav(&fixture("generated/ft8_cq.wav"))
            .expect("decode should not error");
        assert_eq!(decoder.take_last_window_timing(), None);

        // The snapshot is the wrapped Ft8Config plus the neural-OSD weight
        // hash, and nothing PAN-165 added.
        let snapshot = decoder.config_snapshot();
        let mut expected: Vec<String> =
            match serde_json::to_value(pancetta_ft8::Ft8Config::default()).unwrap() {
                serde_json::Value::Object(map) => map.keys().cloned().collect(),
                other => panic!("Ft8Config should serialize to an object, got {other}"),
            };
        expected.push("neural_osd_weights_sha256".to_string());
        expected.sort();
        let mut actual: Vec<String> = snapshot
            .as_object()
            .expect("snapshot is a JSON object")
            .keys()
            .cloned()
            .collect();
        actual.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn receipt_anchor_records_consistent_window_timing() {
        let decoder = Ft8Decoder::with_default_config()
            .with_effort_budget_ms(Some(2000))
            .with_budget_anchor(BudgetAnchor::Receipt);
        let t = receipt_timing(&decoder, "generated/ft8_cq.wav");
        assert!(t.pre_native_ms >= t.waterfall_ms + t.ft8lib_ms, "{t:?}");
        assert_eq!(t.scoped_ms, 0.0, "{t:?}");
        assert!(t.headroom_at_native_ms.unwrap() <= 2000.0, "{t:?}");
        assert!(
            t.window_total_ms >= t.pre_native_ms + t.native_ms - 1.0,
            "{t:?}"
        );
        assert!(t.passes_run >= 1, "{t:?}");
        // The timing is taken, not peeked.
        assert_eq!(decoder.take_last_window_timing(), None);
    }

    #[test]
    fn receipt_anchor_with_qso_partner_runs_scoped_call() {
        let decoder = Ft8Decoder::with_default_config()
            .with_effort_budget_ms(Some(2000))
            .with_budget_anchor(BudgetAnchor::Receipt)
            .with_qso_partner_hz(1500.0);
        let t = receipt_timing(&decoder, "generated/ft8_cq.wav");
        assert!(t.scoped_ms > 0.0, "{t:?}");
        assert!(
            t.pre_native_ms >= t.waterfall_ms + t.scoped_ms + t.ft8lib_ms,
            "{t:?}"
        );
    }

    #[test]
    fn receipt_anchor_without_partner_unlimited_matches_call_anchor_decodes() {
        let path = fixture("generated/ft8_report.wav");
        let messages = |d: &Ft8Decoder| -> std::collections::BTreeSet<String> {
            d.decode_wav(&path)
                .expect("decode should not error")
                .into_iter()
                .map(|d| d.message)
                .collect()
        };
        let receipt = Ft8Decoder::with_default_config().with_budget_anchor(BudgetAnchor::Receipt);
        let plain = Ft8Decoder::with_default_config();
        let receipt_messages = messages(&receipt);
        assert!(!receipt_messages.is_empty());
        assert_eq!(receipt_messages, messages(&plain));
        let t = receipt.take_last_window_timing().unwrap();
        assert_eq!(t.headroom_at_native_ms, None);
        assert_eq!(t.over_deadline_ms, 0.0);
    }

    #[test]
    fn receipt_anchor_refuses_a_non_plain_decode_path() {
        let recent_call_ctx = pancetta_ft8::ApContext {
            my_call: pancetta_ft8::MyCallAp::new("W1PAN"),
            recent_calls: vec![pancetta_ft8::RecentCallAp::new("K1ABC", 0.0).unwrap()],
            active_qso: None,
            active_qsos: Vec::new(),
        };
        let refused = [
            Ft8Decoder::with_default_config()
                .with_budget_anchor(BudgetAnchor::Receipt)
                .with_rolling_window(8),
            Ft8Decoder::with_default_config()
                .with_budget_anchor(BudgetAnchor::Receipt)
                .with_ap_context(recent_call_ctx),
            Ft8Decoder::with_default_config()
                .with_budget_anchor(BudgetAnchor::Receipt)
                .with_two_stage(true),
        ];
        for decoder in refused {
            let err = decoder
                .decode_wav(&fixture("generated/ft8_cq.wav"))
                .unwrap_err();
            assert!(err.to_string().contains("receipt"), "{err}");
            assert_eq!(decoder.take_last_window_timing(), None);
        }
    }

    #[test]
    fn receipt_anchor_accepts_a_my_call_only_ap_context() {
        let ctx = pancetta_ft8::ApContext {
            my_call: pancetta_ft8::MyCallAp::new("W1PAN"),
            recent_calls: Vec::new(),
            active_qso: None,
            active_qsos: Vec::new(),
        };
        let decoder = Ft8Decoder::with_default_config()
            .with_budget_anchor(BudgetAnchor::Receipt)
            .with_ap_context(ctx);
        let decodes = decoder
            .decode_wav(&fixture("generated/ft8_cq.wav"))
            .expect("a my_call-only AP context is production's cold-start context");
        assert!(!decodes.is_empty());
        assert!(decoder.take_last_window_timing().is_some());
    }

    fn fixture_samples(rel: &str) -> Vec<f32> {
        let mut reader = hound::WavReader::open(fixture(rel)).expect("fixture opens");
        match reader.spec().sample_format {
            hound::SampleFormat::Int => reader
                .samples::<i16>()
                .map(|s| s.unwrap() as f32 / 32768.0)
                .collect(),
            hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
        }
    }

    /// PAN-110: the merged mode returns exactly what the shared production
    /// merge makes of the two backends' decodes for the same window.
    #[test]
    fn receipt_anchor_ft8lib_merge_equals_the_shared_production_merge() {
        let rel = "generated/ft8_cq.wav";
        let decoder = Ft8Decoder::with_default_config()
            .with_budget_anchor(BudgetAnchor::Receipt)
            .with_ft8lib_merge(true);
        let merged: Vec<String> = decoder
            .decode_wav(&fixture(rel))
            .expect("merged decode should not error")
            .into_iter()
            .map(|d| d.message)
            .collect();

        let samples = fixture_samples(rel);
        let ft8lib = pancetta_ft8::Ft8Decoder::decode_window_ft8lib_protocol(
            &samples,
            pancetta_ft8::Protocol::Ft8,
        );
        let mut native_decoder =
            pancetta_ft8::Ft8Decoder::new(pancetta_ft8::Ft8Config::default()).unwrap();
        let (native, _) = native_decoder
            .decode_window_with_ap_scoped_partner_budgeted(
                &samples,
                &pancetta_ft8::ApContext::default(),
                None,
                None,
                pancetta_ft8::DecodeBudget::unlimited(),
            )
            .unwrap();
        let expected_texts = DecoderTexts {
            native: native.iter().map(|m| m.text.clone()).collect(),
            ft8lib: ft8lib.iter().map(|m| m.text.clone()).collect(),
        };
        let (expected, _) = pancetta_ft8::merge::drop_unresolved_hash_twins(
            pancetta_ft8::merge::merge_ft8lib_first(ft8lib, native).messages,
            pancetta_ft8::Protocol::Ft8,
        );
        let expected: Vec<String> = expected.into_iter().map(|m| m.text).collect();

        assert!(!merged.is_empty());
        assert_eq!(merged, expected);
        assert_eq!(
            decoder.take_last_window_decoder_texts(),
            Some(expected_texts)
        );
        // Taken, not peeked.
        assert_eq!(decoder.take_last_window_decoder_texts(), None);
    }

    #[test]
    fn without_ft8lib_merge_no_decoder_texts_are_recorded() {
        let decoder = Ft8Decoder::with_default_config().with_budget_anchor(BudgetAnchor::Receipt);
        decoder
            .decode_wav(&fixture("generated/ft8_cq.wav"))
            .expect("decode should not error");
        assert_eq!(decoder.take_last_window_decoder_texts(), None);
    }

    #[test]
    fn qso_filter_bin_range_matches_production_formula() {
        // 1500 Hz / 6.25 = bin 240; ceil(60 / 6.25) = 10 bins either side.
        assert_eq!(qso_filter_bin_range(1500.0), Some(230..=250));
        assert_eq!(qso_filter_bin_range(0.0), None);
        assert_eq!(qso_filter_bin_range(f64::NAN), None);
    }
}

// Keep the preserved V2 recorder separate from the accepted native AP history.
struct TimingLookup {
    legacy: Option<std::sync::Arc<pancetta_ft8::InMemoryDtHistory>>,
    fresh: Option<std::sync::Arc<pancetta_ft8::InMemoryDtHistory>>,
}
impl pancetta_ft8::DtPriorLookup for TimingLookup {
    fn prior(&self, call: &str) -> Option<pancetta_ft8::DtPrior> {
        self.legacy.as_ref().and_then(|h| h.prior(call))
    }
    fn priors_near_freq(&self, hz: f64, radius: f64) -> Vec<pancetta_ft8::DtPrior> {
        self.legacy
            .as_ref()
            .map(|h| h.priors_near_freq(hz, radius))
            .unwrap_or_default()
    }
    fn prior_at(&self, call: &str, at: std::time::SystemTime) -> Option<pancetta_ft8::DtPrior> {
        self.fresh.as_ref().and_then(|h| h.prior_at(call, at))
    }
}

fn record_ap_timing_slot<'a>(
    history: &pancetta_ft8::InMemoryDtHistory,
    messages: impl Iterator<Item = &'a pancetta_ft8::DecodedMessage>,
    at: std::time::SystemTime,
) {
    let mut by_sender: std::collections::BTreeMap<String, Vec<f64>> = Default::default();
    for message in messages {
        if !message.time_offset.is_finite()
            || pancetta_qso::callsign_continuity::has_high_risk_fp_pattern(&message.text)
        {
            continue;
        }
        if let Some(call) = &message.message.from_callsign {
            let call = pancetta_ft8::dt_history::normalize_dt_callsign(call);
            if !call.is_empty() && call != "CQ" {
                by_sender.entry(call).or_default().push(message.time_offset);
            }
        }
    }
    for (call, mut times) in by_sender {
        times.sort_by(f64::total_cmp);
        if times.last().unwrap() - times[0] <= 0.6 {
            // One accepted observation per sender per slot. Never average
            // conflicting sources or count preliminary/main duplicates twice.
            history.record(&call, times[times.len() / 2], at);
        }
    }
}

#[cfg(test)]
mod pan160_history_tests {
    use super::*;
    use pancetta_ft8::DtPriorLookup;
    #[test]
    fn accepted_slot_history_is_independent_deduplicated_and_not_truth_seeded() {
        let a = Ft8Decoder::with_default_config().with_ap_dt_history(false);
        let b = Ft8Decoder::with_default_config().with_ap_dt_history(true);
        let at = std::time::UNIX_EPOCH + Duration::from_secs(1000);
        let message = pancetta_ft8::DecodedMessage::new(
            pancetta_ft8::message::Ft8Message::from_text("CQ W1AW FN31"),
            -10.,
            0.9,
            1000.,
            0.32,
        );
        let history = a.ap_dt_history.as_ref().unwrap();
        record_ap_timing_slot(history, [&message, &message].into_iter(), at);
        assert!(history.prior_at("W1AW", at).is_none());
        assert!(b.ap_dt_history.as_ref().unwrap().is_empty());
        record_ap_timing_slot(
            history,
            [&message].into_iter(),
            at + Duration::from_secs(15),
        );
        assert!(
            history.prior_at("W1AW", at).is_none(),
            "future slot cannot leak into earlier replay"
        );
        assert_eq!(
            history
                .prior_at("W1AW", at + Duration::from_secs(30))
                .unwrap()
                .sighting_count,
            2
        );
        assert_eq!(b.config_snapshot()["ap_dt_history_enabled"], true);
        assert_eq!(b.config_snapshot()["dt_history_enabled"], false);
        let mut conflict = message.clone();
        conflict.time_offset = 1.8;
        record_ap_timing_slot(
            b.ap_dt_history.as_ref().unwrap(),
            [&message, &conflict].into_iter(),
            at,
        );
        assert!(b.ap_dt_history.as_ref().unwrap().is_empty());
    }
}
