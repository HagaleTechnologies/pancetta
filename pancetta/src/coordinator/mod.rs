//! # Application Coordinator
//!
//! The Application Coordinator is the central orchestrator for the Pancetta application.
//! It manages the lifecycle of all components and coordinates communication between them.
//!
//! ## Architecture
//!
//! The coordinator uses point-to-point crossbeam channels for the core data path:
//!   Audio -> DSP -> FT8 Decoder -> TUI
//!
//! The message bus is retained for control messages and health monitoring.
//!
//! ## WAV Playback Mode
//!
//! When started with `--wav <file>`, the coordinator reads a WAV file, resamples to
//! 12 kHz mono, feeds the samples through the DSP/FT8 pipeline, prints decoded messages,
//! and exits.

mod audio;
mod autonomous;
mod dsp;
mod dx_cluster;
mod ft8;
mod hamlib;
mod health;
mod pipeline;
mod psk_reporter;
mod qso;
mod qso_filter;
mod remote_gateway;
mod shutdown;
mod station_agent;
mod tier;
mod tui_relay;
mod tx;
mod util;
mod wav_playback;

pub use tx::{
    coalesce_transmit_requests, remote_tx_permitted, resolve_required_parity, schedule_tx,
    CoalesceEntry, CoalesceOutcome, TxSchedule,
};

pub use qso::compute_manual_tx_offset;

// Re-export the C19 config-reload classifier (safe-live vs deferred) and the
// C20 RF-present/no-decode detector so the coordinator-robustness integration
// tests can exercise the real production decision logic.
pub use health::{
    classify_config_reload, ConfigReloadApplicability, HealthEdges, RfNoDecodeMonitor,
};

/// Canonical key for the `active_tx_qsos` set: QSO ids are compared
/// case-insensitively (and trimmed) so the producer (QSO component) and
/// consumer (TX worker) never disagree on casing. Centralized here so the
/// insert / remove / membership-test sites can't drift.
pub fn active_tx_qso_key(qso_id: &str) -> String {
    qso_id.trim().to_uppercase()
}

/// Current Unix time in whole milliseconds (0 if the clock is before the
/// epoch, which never happens in practice). Used for the lock-free
/// `last_audio_timestamp` atomic (Pass 1 / A10).
pub(crate) fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Decide whether a `TransmitRequest`/`MultiTransmitRequest` item may be
/// keyed, given its `qso_id` and the current active-QSO set.
///
/// Returns `true` (transmit) when:
///   - the item has no `qso_id` (manual free-text / tune / test-TX — never
///     gated), or
///   - the item's `qso_id` is present in `active` (the QSO is still live, or
///     within its post-completion grace window).
///
/// Returns `false` (drop) only when the item belongs to a QSO that is no
/// longer in the active set — i.e. it was superseded / cancelled / failed /
/// timed out, or its completion grace already elapsed.
pub fn tx_qso_is_live(qso_id: Option<&str>, active: &HashSet<String>) -> bool {
    match qso_id {
        None => true,
        Some(id) => active.contains(&active_tx_qso_key(id)),
    }
}

/// The most recent transmit intent the QSO component has produced for a
/// given QSO, keyed by [`active_tx_qso_key`]. Written by the QSO component
/// each time it forwards a `QsoEvent::MessageToSend` as a `TransmitRequest`;
/// read by the TX worker at key-time so it can "pivot" to the freshest
/// message for the QSO if a newer decode advanced the exchange during the
/// worker's (up to ~30 s) pre-PTT wait. See `latest_tx_intent`.
#[derive(Debug, Clone, PartialEq)]
pub struct LatestTxIntent {
    pub message_text: String,
    pub frequency_offset: f64,
    pub tx_parity: Option<pancetta_core::slot::SlotParity>,
}

/// Decide whether the TX worker should swap the message it is about to key
/// for a fresher one from [`LatestTxIntent`]. Returns `Some(intent)` only
/// when there is a live intent for this `qso_id` whose `message_text`
/// differs from what the worker currently holds (a genuine ladder advance
/// or content change) — never for an identical keep-call re-send, and never
/// for `qso_id == None` (manual / tune / test-TX are never pivoted).
pub fn tx_pivot_target(
    qso_id: Option<&str>,
    current_text: &str,
    latest: &HashMap<String, LatestTxIntent>,
) -> Option<LatestTxIntent> {
    let id = qso_id?;
    let intent = latest.get(&active_tx_qso_key(id))?;
    if intent.message_text == current_text {
        None
    } else {
        Some(intent.clone())
    }
}

/// Threshold (Hz) for treating a same-band (or out-of-band) dial move as a
/// "band change" for the C9 active-QSO teardown. Sized to ride over normal
/// fine-tuning / passband nudges within an FT8 sub-band (a few kHz) while
/// still catching a deliberate jump that lands the rig somewhere a QSO can no
/// longer complete. 100 kHz.
pub const BAND_CHANGE_HZ_THRESHOLD: u64 = 100_000;

/// Decide whether a dial-frequency change (`old_hz` → `new_hz`) is a genuine
/// **band change** that should tear down active QSOs (C9), versus a tiny tune
/// wobble that should not.
///
/// Returns `true` when:
///   - the two frequencies map to **different ham bands**
///     ([`pancetta_core::Band::from_frequency`]), or
///   - one/both are outside any ham band but the dial moved more than
///     [`BAND_CHANGE_HZ_THRESHOLD`].
///
/// Returns `false` when:
///   - `old_hz == 0` (uninitialized / first frequency set at startup — there
///     is nothing to tear down, and we must not fire on the initial dial read),
///   - the frequencies are in the **same** ham band (intra-band fine-tuning),
///     or
///   - both are out-of-band and within the threshold (small wobble).
pub fn is_band_change(old_hz: u64, new_hz: u64) -> bool {
    // Nothing to compare against at startup / before the first real read.
    if old_hz == 0 {
        return false;
    }
    if old_hz == new_hz {
        return false;
    }
    match (
        pancetta_core::Band::from_frequency(old_hz),
        pancetta_core::Band::from_frequency(new_hz),
    ) {
        // Both map to a known band: a change iff the band differs.
        (Some(a), Some(b)) => a != b,
        // One/both out-of-band: fall back to a magnitude threshold so a big
        // jump still triggers teardown but a small nudge near a band edge does
        // not.
        _ => old_hz.abs_diff(new_hz) >= BAND_CHANGE_HZ_THRESHOLD,
    }
}

/// Settle window (ms) after a pancetta-initiated frequency command during which
/// the hamlib dial-poll loop suppresses its own C9 teardown.
///
/// When the TUI / autonomous operator commands a band change it updates the
/// shared dial frequency and fires the `BandChanged` teardown *itself*. The
/// rig then takes a moment to slew; until it reaches the commanded frequency
/// the poll loop may read the **old** frequency once or twice. Without this
/// window the poll would mistake that stale reading for an operator dial move
/// *back* to the old band and fire a second (spurious) teardown. Sized to
/// comfortably cover a rigctld round-trip plus VFO slew at the 500 ms poll
/// cadence.
pub const FREQ_COMMAND_SETTLE_MS: u64 = 3_000;

/// Decide whether a band change the **dial-poll loop** just observed
/// (`last_seen_hz` → `polled_hz`) is attributable to a frequency change
/// **pancetta itself commanded**, and therefore must NOT be torn down again by
/// the poll loop (the TUI / autonomous site already fired the `BandChanged`
/// teardown).
///
/// `last_command` is the coordinator's `last_freq_command` anchor: the
/// `(target_hz, issued_at)` of the most recent pancetta-initiated
/// `SetFrequency`, or `None` if pancetta has never commanded a change.
///
/// Returns `true` (suppress the poll teardown) when EITHER:
///   - the polled frequency has reached the commanded frequency's band — the
///     rig settled onto what pancetta asked for (the common, post-settle case);
///     or
///   - the command was issued within [`FREQ_COMMAND_SETTLE_MS`] — the rig is
///     still slewing and the poll may be reading a transient old frequency.
///
/// Returns `false` (the poll loop should treat it as a real operator dial move
/// and fire the teardown) when there is no recent command that can explain the
/// observed band — i.e. the operator turned the rig's dial directly.
pub fn band_change_attributable_to_command(
    polled_hz: u64,
    last_command: Option<(u64, Instant)>,
    now: Instant,
) -> bool {
    let Some((commanded_hz, issued_at)) = last_command else {
        // pancetta never commanded a change → this can only be the operator.
        return false;
    };
    // Still within the post-command settle window: the rig may be slewing and
    // the poll may read a transient stale frequency — suppress.
    if now.duration_since(issued_at) < Duration::from_millis(FREQ_COMMAND_SETTLE_MS) {
        return true;
    }
    // Settled: suppress only if the rig actually reached the commanded band
    // (so the poll reading the commanded frequency back doesn't double-fire).
    // A genuine later operator move to a *different* band is NOT suppressed.
    !is_band_change(commanded_hz, polled_hz)
}

/// Lead-in (seconds) before the UTC slot boundary at which the live decode
/// window starts. The DSP pipeline slices the emitted window so that sample 0
/// corresponds to `slot_boundary − WINDOW_LEAD_SECS`, and the FT8 pipeline
/// subtracts this same lead from every decoded message's `time_offset` so the
/// reported DT is boundary-relative (≈0 for a station transmitting on the
/// boundary). The lead-in gives a small margin for stations that start a touch
/// early (FT8 DT can be slightly negative) and absorbs emit-trigger jitter.
/// Shared between `dsp.rs` (window slice anchor) and `ft8.rs` (DT correction)
/// so the two halves of the fix can never drift apart.
pub(crate) const WINDOW_LEAD_SECS: f64 = 0.5;

/// The single 12 kHz sample rate the decode/DSP path is anchored to. The DSP
/// thread decimates the incoming audio to this rate before windowing; all
/// derived sample counts (`DspTiming`) are expressed at this rate.
pub(crate) const FT8_SAMPLE_RATE: usize = 12000;

/// Map the config-local [`pancetta_config::OperatingMode`] to a concrete
/// [`pancetta_ft8::Protocol`]. The config crate intentionally does not depend
/// on `pancetta-ft8`, so the coordinator owns this mapping. Derived once at
/// startup from `rig_config.operating_mode()`.
pub(crate) fn protocol_from_mode(mode: pancetta_config::OperatingMode) -> pancetta_ft8::Protocol {
    match mode {
        pancetta_config::OperatingMode::Ft8 => pancetta_ft8::Protocol::Ft8,
        pancetta_config::OperatingMode::Ft4 => pancetta_ft8::Protocol::Ft4,
        // FT2 is feature-gated in pancetta-ft8 behind `ft2`. When the feature
        // is off, fall back to FT8 timing (the only mode actually wired); the
        // config validator already accepts "FT2", so this keeps a stray FT2
        // config from panicking on a host built without the feature.
        #[cfg(feature = "ft2")]
        pancetta_config::OperatingMode::Ft2 => pancetta_ft8::Protocol::Ft2,
        #[cfg(not(feature = "ft2"))]
        pancetta_config::OperatingMode::Ft2 => pancetta_ft8::Protocol::Ft8,
    }
}

/// Canonical ADIF/display mode string for the station-wide
/// [`pancetta_config::OperatingMode`]. Stamped into [`QsoMetadata::mode`]
/// (→ ADIF `MODE`) and the TUI decode view. Derived once at startup from
/// `rig_config.operating_mode()`; defaults to `"FT8"` on parse error.
pub(crate) fn mode_str(mode: pancetta_config::OperatingMode) -> &'static str {
    match mode {
        pancetta_config::OperatingMode::Ft8 => "FT8",
        pancetta_config::OperatingMode::Ft4 => "FT4",
        pancetta_config::OperatingMode::Ft2 => "FT2",
    }
}

/// Per-protocol timing values the DSP windowing thread needs, derived once at
/// startup from a [`pancetta_ft8::ProtocolParams`] via [`derive_dsp_timing`].
///
/// Threading these in (rather than hardcoding FT8 constants in the DSP thread)
/// lets mode=FT4 run a 5.04s window + 6.5s decode phase + 7.5s overlap while
/// mode=FT8 stays byte-identical to the historical 12.64 / 13 / 15×rate values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DspTiming {
    /// Decode window length in 12 kHz samples (FT8 → 151_680, FT4 → 60_480).
    pub window_samples: usize,
    /// Phase past the slot boundary at which the decode fires (FT8 → 13s,
    /// FT4 → 6.5s): `cycle_duration − decode_margin`.
    pub decode_phase: chrono::Duration,
    /// Overlap retained between windows in 12 kHz samples — one full cycle
    /// (FT8 → 180_000 = 15×rate, FT4 → 90_000 = 7.5×rate).
    pub overlap_samples: usize,
    /// Slot length in nanoseconds (FT8 → 15e9, FT4 → 7.5e9). Fed to the
    /// `*_with_period` slot helpers.
    pub slot_ns: i64,
}

/// Margin (seconds) subtracted from the protocol's `cycle_duration` to get the
/// decode-trigger phase: the decode fires this long *before* the next slot
/// boundary, after the transmission's symbols have ended (FT8 tx is 12.64s of a
/// 15s cycle → 2.0s margin → fire at 13s; FT4 tx is 5.04s of a 7.5s cycle, so a
/// 1.0s margin fires at 6.5s, comfortably after symbol-end with headroom for the
/// QSO state machine before the next TX boundary).
fn decode_margin_secs(protocol: pancetta_ft8::Protocol) -> f64 {
    match protocol {
        pancetta_ft8::Protocol::Ft8 => 2.0,
        _ => 1.0,
    }
}

/// Derive the DSP windowing timing from the active protocol's parameters.
///
/// Pure function (no clock, no I/O) so it is unit-testable. The FT8 invariant is
/// the hard regression guard: for `ProtocolParams::ft8()` this MUST yield the
/// historical 12.64s window (151_680 samples), 13s decode phase, and 15×rate
/// (180_000) overlap — see `derive_dsp_timing_ft8_byte_identical`.
pub(crate) fn derive_dsp_timing(pp: &pancetta_ft8::ProtocolParams) -> DspTiming {
    // Window covers the transmission's symbols: num_symbols × symbol_period.
    let window_seconds = pp.num_symbols as f64 * pp.symbol_period;
    let window_samples = (FT8_SAMPLE_RATE as f64 * window_seconds) as usize;

    // Decode fires `decode_margin` before the next boundary.
    let decode_phase_secs = pp.cycle_duration - decode_margin_secs(pp.protocol);
    let decode_phase = chrono::Duration::nanoseconds((decode_phase_secs * 1e9) as i64);

    // Overlap retained between windows = one full cycle.
    let overlap_seconds = pp.cycle_duration;
    let overlap_samples = (FT8_SAMPLE_RATE as f64 * overlap_seconds) as usize;

    DspTiming {
        window_samples,
        decode_phase,
        overlap_samples,
        slot_ns: pp.slot_ns(),
    }
}

/// How recently the operator must have interacted with the console for the
/// autonomous engine to be allowed to INITIATE contact (call CQ / pounce).
///
/// FCC §97.221: a station under *automatic* control (no control operator at the
/// control point) must not ORIGINATE on the standard FT8 frequencies — it may
/// only respond to interrogation. We treat any console keypress as proof the
/// operator is present (local control), which lifts the initiation restriction
/// for this window. Headless or idle → presence goes stale → respond-only
/// initiation (in-progress QSOs still continue). See
/// `docs/fcc-part97-compliance.md`.
pub(crate) const OPERATOR_PRESENCE_WINDOW: Duration = Duration::from_secs(120);

/// `true` if the operator interacted with the console within
/// [`OPERATOR_PRESENCE_WINDOW`]. `last_input_ms` holds Unix-epoch milliseconds
/// of the last console keypress (0 = never seen / headless).
pub(crate) fn operator_present_now(last_input_ms: &std::sync::atomic::AtomicU64) -> bool {
    let last = last_input_ms.load(std::sync::atomic::Ordering::Relaxed);
    if last == 0 {
        return false;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now_ms.saturating_sub(last) < OPERATOR_PRESENCE_WINDOW.as_millis() as u64
}

use anyhow::Result;
use pancetta_config::Config;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

use util::resample_linear;

/// Application coordinator that manages all Pancetta components
pub struct ApplicationCoordinator {
    /// Unique instance identifier
    id: uuid::Uuid,

    /// Application configuration (hot-reloadable)
    config: Arc<RwLock<Config>>,

    /// Central message bus for inter-component communication
    message_bus: MessageBus,

    /// Component managers
    ft8_decoder: Option<Ft8Decoder>,

    /// Named component task handles for health monitoring
    named_task_handles: Vec<(ComponentId, JoinHandle<Result<()>>)>,

    /// Component health status map (shared with health monitor task)
    component_status: Arc<RwLock<HashMap<ComponentId, ComponentStatus>>>,

    /// Managed rigctld child process (killed on shutdown)
    #[cfg(feature = "pancetta-hamlib")]
    rigctld_process: Option<std::process::Child>,

    /// Application state
    is_running: Arc<AtomicBool>,
    shutdown_signal: Arc<AtomicBool>,
    /// Operator-requested abort of the in-flight TX without exiting.
    /// F8 → TUI sets this true; the TX worker's interruptible_sleep wakes,
    /// drops PttGuard (PTT-off), sends TransmitComplete failure, resets
    /// the flag at the start of the next message, and continues.
    /// Distinct from `shutdown_signal` (which means "stop the whole app").
    abort_current_tx: Arc<AtomicBool>,
    /// hb-161 — Phase 5 emergency-stop runtime gate. Set to `true` at
    /// startup based on `config.autonomous.enabled`. Cleared (set to
    /// `false`) when the operator presses Shift+Q in the TUI; the
    /// autonomous decision loop reads this every cycle and skips
    /// `TransmitRequest` dispatch when it's false. Toggled back on by
    /// the autonomous TUI command (`a`) or by re-pressing Shift+Q (the
    /// latter is reserved for future use; today it's one-shot off).
    /// Separate from `shutdown_signal` and `abort_current_tx`.
    autonomous_enabled_runtime: Arc<AtomicBool>,

    /// Global tri-state TX policy (`pancetta_core::TxPolicy` encoded via
    /// `as_u8`/`from_u8`). Initialized to `Full`. Orthogonal to
    /// `autonomous_enabled_runtime`: the autonomous-initiation gate
    /// requires BOTH the autonomous runtime gate open AND the policy to
    /// allow initiation. Read by:
    ///   - the TX worker (`tx.rs`) as the hard mute — `Disabled` consumes
    ///     a `TransmitRequest`/`MultiTransmitRequest` without keying PTT;
    ///   - the autonomous loop (`autonomous.rs`) — `RespondOnly`/`Disabled`
    ///     suppress autonomous *initiations* (CQ-self, hunt/pounce);
    ///   - the command relay (`tui_relay.rs`) — refuses `StartCq` and
    ///     `CallStation` when the policy is not `Full`, and echoes the
    ///     policy back to the TUI banner.
    ///
    /// QSO-in-progress messages and answering callers stay allowed under
    /// `RespondOnly` (only `Disabled` mutes them, at the TX worker).
    tx_policy: Arc<std::sync::atomic::AtomicU8>,

    /// Operator TX-frequency mode (`pancetta_core::TxFreqMode` as `u8`),
    /// default `Hold`. Shared with the QSO engine (gates the stuck-DX TX-offset
    /// hop) and the autonomous operator (gates the smart-frequency allocator and
    /// collision-listen jitter). In `Hold` the operator's picked offset is
    /// sticky; `Auto` lets pancetta choose/adjust it. Toggled from the TUI
    /// (`f`). Orthogonal to [`Self::tx_policy`].
    tx_freq_mode: Arc<std::sync::atomic::AtomicU8>,

    /// Unix-epoch milliseconds of the operator's last console keypress (0 =
    /// never / headless). Stamped by the TUI key handler, read by the autonomous
    /// engine's FCC §97.221 presence gate (see [`operator_present_now`]).
    last_operator_input_ms: Arc<std::sync::atomic::AtomicU64>,
    startup_time: Instant,

    /// Configuration
    audio_device: Option<String>,
    no_audio: bool,
    headless: bool,
    enable_metrics: bool,
    metrics_port: u16,

    /// WAV file playback path (if set, runs in playback mode)
    wav_path: Option<PathBuf>,

    /// One-shot test transmission. If Some, after startup the coordinator
    /// injects a single TransmitRequest with this message text and shuts
    /// down on TransmitComplete. Used for hardware bench validation.
    test_tx: Option<String>,
    test_tx_offset: f64,

    /// Cached station lookup for priority scoring (shared between QSO and autonomous components).
    cached_lookup: std::sync::Arc<crate::priority_evaluator::CachedStationLookup>,

    /// cqdx.io integration bridge (None = degraded mode).
    cqdx_bridge: Option<std::sync::Arc<crate::cqdx_bridge::CqdxBridge>>,

    /// Sender for waterfall data to the autonomous operator.
    waterfall_to_auto_tx: Option<crossbeam_channel::Sender<Vec<Vec<f32>>>>,

    /// Shared active QSO AP state for FT8 AP3/AP4 decoding.
    /// Updated by the QSO component, read by the FT8 decoder thread.
    active_qso_ap: std::sync::Arc<std::sync::RwLock<Option<pancetta_ft8::QsoAp>>>,

    /// hb-091 scoped fast-path: most recent active QSO partner's audio
    /// frequency in Hz. Updated by the QSO component alongside
    /// `active_qso_ap`; read by the FT8 decoder thread to scope an
    /// early scoped decode pass at the partner's known location.
    /// `None` when no QSO is active.
    active_qso_freq_hz: std::sync::Arc<std::sync::RwLock<Option<f64>>>,

    /// hb-062 FP filter: applied between decode merge and broadcast in the
    /// FT8 thread. None = filter disabled (default). When enabled, drops
    /// decodes whose extracted callsigns don't appear in operator-ADIF +
    /// rolling-window + cqdx-spotted sources.
    fp_filter: Option<std::sync::Arc<pancetta_qso::CallsignContinuityFilter>>,

    /// Shared cross-slot state (hb-048 a7 / hb-057 DT history / hb-173
    /// within-QSO context substrate). Populated by the FT8 decoder thread
    /// after each successful, FP-filter-accepted decode; consumed by
    /// downstream hypotheses (none yet — SHIPPED-INFRA module). Cloning
    /// the `Arc` is cheap; the container's three inner tables hold their
    /// own `RwLock`s so locks never cross tables.
    cross_time_state: std::sync::Arc<pancetta_qso::CrossTimeState>,

    /// hb-237: cross-sequence A7 callsign cache. Populated by the FT8
    /// decoder thread after each successful, FP-filter-accepted decode
    /// when `Ft8Config::cross_sequence_a7_enabled` is true. The cache
    /// holds the prior slot's decoded callsigns as A7 seed candidates
    /// for the next slot's opposite-parity decode. Default behavior is
    /// inert — the cache populates but no decoder consumer reads from
    /// it yet (per-seed enumeration is a follow-on; see spec ref
    /// `research/specs/spec-wsjtr-cross-sequence-a7.md`).
    cross_sequence_cache: std::sync::Arc<std::sync::RwLock<pancetta_qso::CrossSequenceCallCache>>,

    /// TUI relay OS thread handle (joined on shutdown)
    tui_relay_handle: Option<std::thread::JoinHandle<()>>,

    /// Current operating frequency in Hz, shared across components.
    /// Updated by the hamlib polling task; read by cqdx.io and PSKReporter
    /// to compute absolute RF frequency from audio offsets.
    operating_frequency_hz: Arc<std::sync::atomic::AtomicU64>,

    /// Rig split-TX dial in Hz (0 = simplex). Written by the TUI SetSplit relay,
    /// read by the QSO RF stamp (effective TX dial). RX dial stays
    /// `operating_frequency_hz`.
    split_tx_frequency_hz: Arc<std::sync::atomic::AtomicU64>,

    /// Operator-held manual TX audio offset in Hz (0 = unset / auto). Set by the
    /// TUI 'o' set-offset modal; read by the manual-call handler to place our TX
    /// offset (WSJT-X "Hold Tx Freq" style) instead of defaulting to the DX's freq.
    tx_offset_hold_hz: Arc<std::sync::atomic::AtomicU64>,

    /// Active protocol's slot length in nanoseconds (FT8 → 15e9, FT4 → 7.5e9),
    /// derived once at startup from `[rig].mode`. Read by the decode loop's
    /// parity-stamping sites (`SlotParity::of_with_period`) and the DSP thread.
    /// Held as an atomic to mirror the other shared-timing atomics; it is set
    /// once and not mutated at runtime (a live mode switch is a later task).
    active_slot_ns: Arc<std::sync::atomic::AtomicI64>,

    /// Active digital-mode protocol (FT8 / FT4 / FT2), derived once at startup
    /// from `[rig].mode`. The TX worker branches its encode+modulate on this so
    /// FT4 transmits a 4-GFSK / 105-symbol FT4 waveform (not the FT8 waveform);
    /// `Protocol::Ft8` keeps the exact legacy calls (byte-identical). `Copy`, set
    /// once, not mutated at runtime (a live mode switch is a later task).
    active_protocol: pancetta_ft8::Protocol,

    /// Active protocol's decode phase in nanoseconds (FT8 → 13e9, FT4 → 6.5e9):
    /// how far past the slot boundary the decode window is received. The decode
    /// loop subtracts this from the window-received instant to recover the
    /// containing slot's start before stamping `SlotParity`. Derived once at
    /// startup from `[rig].mode` (the same `DspTiming.decode_phase` the DSP
    /// thread uses); set once, not mutated at runtime. mode=FT8 is 13e9 ns,
    /// byte-identical to the prior hardcoded `Duration::seconds(13)`.
    active_decode_phase_ns: Arc<std::sync::atomic::AtomicI64>,

    /// `true` when the read-only `remote_gateway` component is enabled
    /// (`[network.remote_gateway].enabled`). Cached from config at construction
    /// so the display-event emit sites (decode fan-out, QSO snapshot, freq,
    /// s-meter, TX status, split) can cheaply gate their **additive**
    /// dual-destination send to `ComponentId::RemoteGateway` — when the gateway
    /// is off, the emit sites skip the extra clone+send entirely (the existing
    /// `→Tui`/`→Qso` sends are never touched). See `remote_gateway::relay_to_gateway`.
    gateway_enabled: Arc<AtomicBool>,

    /// `true` when the coordinator is running in Fox (DXpedition operator) mode.
    /// Default `false` (standard Hound / normal operation).
    fox_mode: Arc<AtomicBool>,

    /// Maximum simultaneous caller-answer QSOs while Fox mode is engaged.
    /// Seeded from `config.fox.max_streams` at construction. The QSO
    /// component's `maybe_answer_caller` path uses this value instead of
    /// `auto_answer_max_concurrent` when `fox_mode` is set.
    fox_max_streams: Arc<AtomicUsize>,

    /// `true` while the TX worker has PTT keyed. Set by the TX worker on
    /// key/unkey; read by the hamlib polling task so SWR is only sampled while
    /// transmitting (SWR is only meaningful under forward power) and by the TUI
    /// status bar to show the live reading only during TX.
    pub(crate) ptt_active: Arc<std::sync::atomic::AtomicBool>,

    /// C9 dedup anchor: the most recent dial frequency **pancetta itself
    /// commanded** (TUI `SetFrequency`, autonomous `ChangeBand`) and when.
    /// `None` until the first pancetta-initiated frequency change.
    ///
    /// The hamlib dial-poll loop uses this to distinguish an *operator dial
    /// move* (which it must tear active QSOs down for) from a frequency
    /// change **pancetta initiated** (where the TUI / autonomous site has
    /// already fired the `BandChanged` teardown — the poll must NOT
    /// double-fire when it later reads the commanded frequency back off the
    /// rig, nor on a transient old-frequency reading while the rig is still
    /// settling to the commanded value). See
    /// [`band_change_attributable_to_command`] and the poll loop in
    /// `coordinator/hamlib.rs`.
    last_freq_command: Arc<std::sync::Mutex<Option<(u64, Instant)>>>,

    /// Performance metrics
    message_count: Arc<std::sync::atomic::AtomicU64>,
    /// Last-audio-sample wall-clock timestamp as Unix epoch milliseconds
    /// (0 = no audio yet). Perf (Pass 1 / A10): was `Arc<RwLock<Option<Instant>>>`
    /// written under an async write lock on EVERY audio relay batch (the most
    /// frequent lock in the pipeline) and read only by the 30s stats log — now
    /// a lock-free atomic. Wall-clock ms is fine here: the sole reader formats a
    /// "last audio Xs ago" status string.
    last_audio_timestamp: Arc<std::sync::atomic::AtomicU64>,
    /// Wall-clock ms of the last decode-window completion (0 = never). Written
    /// once per decode window from the decoder thread and read only by the 30s
    /// stats log — a lock-free atomic (Pass-2 / A9), mirroring
    /// `last_audio_timestamp`. Replaces an `RwLock<Option<Instant>>` that the
    /// sync decoder thread had to touch via `rt.block_on` each window.
    last_decode_timestamp: Arc<std::sync::atomic::AtomicU64>,

    /// `true` when the resolved TX OUTPUT device fell back to the system
    /// default rather than an explicit rig CODEC (the classic "PTT keys, audio
    /// on speakers" misconfig). Set by the audio thread once at start; read by
    /// the TUI relay to drive a persistent station-panel badge.
    audio_output_default: Arc<AtomicBool>,

    /// Command channel into the dedicated audio thread for **live device
    /// switching**. The TUI `SelectDevice` handler sends an
    /// [`AudioReopenRequest`](crate::coordinator::audio::AudioReopenRequest)
    /// here; the audio thread tears down and rebuilds the cpal stream(s) on the
    /// new device(s) without a restart and reports success/failure back over the
    /// request's oneshot. `None` until the real (non-stub) audio thread has been
    /// started; absent in stub/`--no-audio` modes (where live switching is a
    /// no-op the handler reports to the operator).
    audio_reopen_tx:
        Option<crossbeam_channel::Sender<crate::coordinator::audio::AudioReopenRequest>>,

    /// Rig connection state for the TUI badge. Encodes
    /// [`crate::coordinator::hamlib::RigConnState`] via `as_u8`/`from_u8`.
    /// Written by the hamlib connect/poll loop, read by the TUI relay.
    rig_conn_state: Arc<std::sync::atomic::AtomicU8>,

    /// hb-216 S2 — scoped-fast-path activation flag. Seeded from
    /// `PANCETTA_SCOPED_FAST_PATH` env var at startup; rewritten by the
    /// hardware-tier probe (background) when it lands. The FT8 hot loop
    /// reads this with a relaxed load each window iteration in lieu of
    /// the prior env-var probe.
    pub(crate) scoped_fast_path: Arc<AtomicBool>,

    /// hb-216 S2 — shared decoder config the FT8 thread reads on each
    /// window iteration. The tier probe may rewrite Slow-tier presets
    /// (`max_decode_passes=1`, `osd_depth=Some(1)`) once it classifies
    /// the host; the FT8 thread rebuilds its decoder when the
    /// `(max_decode_passes, osd_depth)` tuple changes.
    pub(crate) ft8_config: Arc<RwLock<Ft8Config>>,

    /// Non-fatal config-load warnings (e.g. a `pancetta.toml` that existed but
    /// failed to parse and was silently reverted to defaults). Surfaced to the
    /// TUI as an error banner at startup so the operator is never left guessing
    /// why their callsign/audio came up as defaults. Empty on a clean load.
    pub(crate) config_warnings: Vec<String>,

    /// Set of QSO ids (uppercased strings) whose TX is currently *live* —
    /// i.e. the QSO is in a non-terminal active state (or within the brief
    /// post-completion grace window during which its final 73 is still
    /// allowed out). Maintained by the QSO component from the QsoEvent
    /// stream; read by the TX worker, which refuses to key PTT for a
    /// `TransmitRequest` whose `qso_id` is no longer present.
    ///
    /// This is the core defense against the "stale TX keeps transmitting
    /// after a QSO ends" bug: superseding / cancelling / completing a QSO
    /// changes its state but does NOT purge requests already sitting in the
    /// TX path. The TX worker dropping inactive-QSO requests at key-time
    /// closes that gap. Requests with `qso_id == None` (manual free-text,
    /// tune, test-TX) are never gated by this set.
    pub(crate) active_tx_qsos: Arc<std::sync::RwLock<HashSet<String>>>,
    /// Newest transmit intent per QSO (see [`LatestTxIntent`]). Written by
    /// the QSO component as it forwards each `MessageToSend`; read by the TX
    /// worker at key-time to pivot to the freshest message for the QSO.
    pub(crate) latest_tx_intent: Arc<std::sync::RwLock<HashMap<String, LatestTxIntent>>>,
    /// Station-agent remote-TX arm gate. The FINAL TX authority for
    /// remote-originated (`TxOrigin::Remote`) transmit requests: the TX worker
    /// checks `tx_permitted(now_ms)` before keying PTT for any remote request
    /// and drops it (fail-closed) if not permitted. Seeded at startup with the
    /// LOCAL operator consent from `[network.station_agent].remote_tx_enabled`
    /// (default OFF), so with nothing arming it and no remote requests being
    /// constructed (P0–P2), this gate is inert. Local TX never consults it.
    pub(crate) remote_tx_arm: Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
}

#[cfg(feature = "pancetta-hamlib")]
impl Drop for ApplicationCoordinator {
    fn drop(&mut self) {
        if let Some(mut child) = self.rigctld_process.take() {
            eprintln!("Pancetta: killing managed rigctld (PID {})", child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Coordinator configuration
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// Component startup timeout
    pub startup_timeout: Duration,

    /// Component shutdown timeout
    pub shutdown_timeout: Duration,

    /// Health check interval
    pub health_check_interval: Duration,

    /// Message bus buffer size
    pub message_buffer_size: usize,

    /// Maximum concurrent tasks
    pub max_concurrent_tasks: usize,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(10),
            health_check_interval: Duration::from_secs(5),
            message_buffer_size: 10000,
            max_concurrent_tasks: 100,
        }
    }
}

/// Component health status (coordinator-level)
#[derive(Debug, Clone)]
pub struct ComponentHealth {
    pub component_id: ComponentId,
    pub is_healthy: bool,
    pub last_heartbeat: Instant,
    pub error_count: u32,
    pub message_count: u64,
    pub avg_latency_ms: f64,
}

/// State of a component as tracked by the health monitor
#[derive(Debug, Clone, PartialEq)]
pub enum ComponentState {
    /// Component is running normally
    Running,
    /// Component has failed (with error description)
    Failed(String),
    /// Component was never started or is disabled
    NotStarted,
}

impl std::fmt::Display for ComponentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComponentState::Running => write!(f, "Running"),
            ComponentState::Failed(msg) => write!(f, "Failed: {}", msg),
            ComponentState::NotStarted => write!(f, "NotStarted"),
        }
    }
}

/// Per-component status tracked by the coordinator health monitor
#[derive(Debug, Clone)]
pub struct ComponentStatus {
    pub state: ComponentState,
    pub last_seen: Instant,
    pub error_count: u32,
}

impl ComponentStatus {
    fn new_running() -> Self {
        Self {
            state: ComponentState::Running,
            last_seen: Instant::now(),
            error_count: 0,
        }
    }
}

/// Criticality level of a component -- determines shutdown behavior on failure
#[derive(Debug, Clone, Copy, PartialEq)]
enum ComponentCriticality {
    /// Application can continue without this component
    NonCritical,
    /// Component failure should be logged prominently but app continues
    Important,
}

fn component_criticality(id: ComponentId) -> ComponentCriticality {
    match id {
        ComponentId::Ft8Decoder => ComponentCriticality::Important,
        ComponentId::Audio => ComponentCriticality::NonCritical,
        ComponentId::Dsp => ComponentCriticality::Important,
        _ => ComponentCriticality::NonCritical,
    }
}

/// Human-readable degradation message for a failed component
fn degradation_message(id: ComponentId) -> &'static str {
    match id {
        ComponentId::Audio => "Audio disconnected -- no RX/TX until reconnected",
        ComponentId::Hamlib => "Rig control lost -- PTT safety defaulting to OFF",
        ComponentId::DxCluster => "DX cluster disconnected -- continuing without spots",
        ComponentId::Ft8Decoder => "FT8 decoder crashed -- no decoding until restart",
        ComponentId::Dsp => "DSP pipeline failed -- audio processing halted",
        ComponentId::PskReporter => "PSKReporter upload failed -- spots not being reported",
        ComponentId::Qso => "QSO manager failed -- contact logging unavailable",
        ComponentId::Ft8Transmitter => "FT8 transmitter failed -- TX disabled",
        ComponentId::Autonomous => "Autonomous operator failed -- manual operation only",
        _ => "Component failed",
    }
}

impl ApplicationCoordinator {
    /// Create a new application coordinator
    // rationale: the coordinator constructor takes many independent dependencies;
    // a builder/params struct would relocate the same fields without simplifying.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        config: Config,
        audio_device: Option<String>,
        no_audio: bool,
        headless: bool,
        enable_metrics: bool,
        metrics_port: u16,
        wav_path: Option<PathBuf>,
        test_tx: Option<String>,
        test_tx_offset: f64,
        shutdown_signal: Arc<AtomicBool>,
        config_warnings: Vec<String>,
    ) -> Result<Self> {
        let span = span!(Level::INFO, "coordinator_init");
        let _enter = span.enter();

        info!("Initializing Application Coordinator");

        let id = uuid::Uuid::new_v4();
        let startup_time = Instant::now();

        // Create message bus with high-performance configuration
        let coordinator_config = CoordinatorConfig::default();
        let message_bus = MessageBus::new(coordinator_config.message_buffer_size)?;

        // Cache the remote-gateway enabled flag before `config` is moved into
        // the Arc<RwLock> — the additive dual-destination emit sites read this
        // atomic to gate their (cheap, additive) sends to the gateway.
        let gateway_enabled_init = config.network.remote_gateway.enabled;
        // Snapshot the station-agent LOCAL remote-TX consent before config is
        // moved into the Arc<RwLock>. Seeds the remote-TX arm's local-consent
        // gate; default OFF, so remote TX can never be permitted this phase.
        let remote_tx_consent_init = config.network.station_agent.remote_tx_enabled;
        // Snapshot fox.max_streams before config is moved into the Arc<RwLock>.
        // The QSO component reads this to cap concurrent caller-answer QSOs
        // while Fox mode is engaged.
        let fox_max_streams_init = config.fox.max_streams;

        // Derive the active protocol's slot length from [rig].mode before
        // config is moved into the Arc<RwLock>. The config validator already
        // rejected an unknown mode at load time; an Err here would only mean a
        // mode validated-then-mutated, so we fall back to FT8 timing rather
        // than failing coordinator init.
        let active_protocol = match config.rig.operating_mode() {
            Ok(mode) => protocol_from_mode(mode),
            Err(e) => {
                warn!("invalid [rig].mode ({e}); defaulting protocol timing to FT8");
                pancetta_ft8::Protocol::Ft8
            }
        };
        let active_slot_ns_init = active_protocol.slot_ns();
        // Decode phase (ns) the parity-stamping sites subtract to recover the
        // slot start — same value the DSP thread derives (FT8 → 13e9).
        let active_decode_phase_ns_init = derive_dsp_timing(
            &pancetta_ft8::ProtocolParams::from_protocol(active_protocol),
        )
        .decode_phase
        .num_nanoseconds()
        .unwrap_or(13_000_000_000);
        info!(
            "Active digital mode: {} (slot {} ns, decode phase {} ns)",
            active_protocol, active_slot_ns_init, active_decode_phase_ns_init
        );

        // Wrap config in Arc<RwLock> for hot-reloading
        let config = Arc::new(RwLock::new(config));

        // hb-216 S2: shared FT8 decoder config + scoped-fast-path atomic.
        // `tier::initialize` seeds the atomic from env, reads the on-disk
        // cache if present, and spawns a background probe on cache miss.
        // The FT8 hot loop reads both fields without blocking on probe
        // completion.
        // Seed the shared decoder config with the active protocol from
        // [rig].mode so the FT8 hot loop actually demodulates FT4 (4-GFSK / FT4
        // Costas) in FT4 mode, not just retunes the slot grid. Without this the
        // decoder would run FT8 geometry against FT4 audio. mode=FT8 →
        // protocol=Ft8, byte-identical to the previous `Ft8Config::default()`.
        let ft8_config = Arc::new(RwLock::new(Ft8Config {
            protocol: active_protocol,
            ..Ft8Config::default()
        }));
        let scoped_fast_path = tier::initialize(ft8_config.clone()).await;

        let coordinator = Self {
            id,
            config,
            message_bus,
            ft8_decoder: None,
            named_task_handles: Vec::new(),
            component_status: Arc::new(RwLock::new(HashMap::new())),
            is_running: Arc::new(AtomicBool::new(false)),
            shutdown_signal,
            abort_current_tx: Arc::new(AtomicBool::new(false)),
            // Initial value is overwritten in start_autonomous_component
            // once config.autonomous.enabled is read. Start `true` so a
            // Q-press before component start still records the operator's
            // intent (the autonomous start path also respects this gate).
            autonomous_enabled_runtime: Arc::new(AtomicBool::new(true)),
            // Default global TX policy = Full (initiate + respond, the
            // historical behavior). Operator cycles it from the TUI.
            tx_policy: Arc::new(std::sync::atomic::AtomicU8::new(
                pancetta_core::TxPolicy::default().as_u8(),
            )),
            // Default TX-frequency mode = Hold (operator's picked offset is
            // sticky; pancetta never moves it autonomously). Operator switches
            // to Auto from the TUI (`f`).
            tx_freq_mode: Arc::new(std::sync::atomic::AtomicU8::new(
                pancetta_core::TxFreqMode::default().as_u8(),
            )),
            // 0 = no console input seen yet → not present → respond-only
            // initiation until the operator touches the keyboard.
            last_operator_input_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            startup_time,
            audio_device,
            no_audio,
            headless,
            enable_metrics,
            metrics_port,
            wav_path,
            test_tx,
            test_tx_offset,
            cached_lookup: std::sync::Arc::new(
                crate::priority_evaluator::CachedStationLookup::new(),
            ),
            cqdx_bridge: None,
            waterfall_to_auto_tx: None,
            active_qso_ap: std::sync::Arc::new(std::sync::RwLock::new(None)),
            active_qso_freq_hz: std::sync::Arc::new(std::sync::RwLock::new(None)),
            fp_filter: None,
            cross_time_state: std::sync::Arc::new(pancetta_qso::CrossTimeState::empty()),
            cross_sequence_cache: std::sync::Arc::new(std::sync::RwLock::new(
                pancetta_qso::CrossSequenceCallCache::default(),
            )),
            tui_relay_handle: None,
            // Initialize to 0 — hamlib will read the actual rig frequency on startup.
            // If hamlib isn't available, the TUI default (14.074) takes over.
            operating_frequency_hz: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            // 0 = simplex; the TUI SetSplit relay writes the split TX dial when
            // the operator enables split mode on the rig.
            split_tx_frequency_hz: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            // 0 = unset / auto; the TUI 'o' modal writes the operator-held
            // TX audio offset when the operator pins a specific frequency.
            tx_offset_hold_hz: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            // Active protocol slot length, derived from [rig].mode above
            // (default 15e9 for FT8). Set once; read by the decode/DSP paths.
            active_slot_ns: Arc::new(std::sync::atomic::AtomicI64::new(active_slot_ns_init)),
            // Active digital-mode protocol from [rig].mode (default Ft8). The TX
            // worker branches encode+modulate on this; Ft8 stays byte-identical.
            active_protocol,
            // Decode phase the parity-stamping sites subtract (FT8 → 13e9 ns).
            active_decode_phase_ns: Arc::new(std::sync::atomic::AtomicI64::new(
                active_decode_phase_ns_init,
            )),
            gateway_enabled: Arc::new(AtomicBool::new(gateway_enabled_init)),
            fox_mode: Arc::new(AtomicBool::new(false)),
            fox_max_streams: Arc::new(AtomicUsize::new(fox_max_streams_init)),
            ptt_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            // C9 dedup anchor — no pancetta-initiated frequency command yet.
            last_freq_command: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(feature = "pancetta-hamlib")]
            rigctld_process: None,
            message_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_audio_timestamp: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_decode_timestamp: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            audio_output_default: Arc::new(AtomicBool::new(false)),
            audio_reopen_tx: None,
            rig_conn_state: Arc::new(std::sync::atomic::AtomicU8::new(
                crate::coordinator::hamlib::RigConnState::default().as_u8(),
            )),
            scoped_fast_path,
            ft8_config,
            config_warnings,
            active_tx_qsos: Arc::new(std::sync::RwLock::new(HashSet::new())),
            latest_tx_intent: Arc::new(std::sync::RwLock::new(HashMap::new())),
            // Station-agent remote-TX arm. Fresh (unarmed, not killed), then
            // seed the LOCAL operator consent from config. With consent OFF (the
            // default) `tx_permitted()` is always false; nothing arms and no
            // remote request is constructed in P0–P2, so the gate is inert.
            remote_tx_arm: {
                let mut st = pancetta_agent::arm::ArmState::new();
                let now_ms = chrono::Utc::now().timestamp_millis();
                let _ = st.set_local_consent(remote_tx_consent_init, now_ms);
                Arc::new(std::sync::Mutex::new(st))
            },
        };

        info!("Application Coordinator initialized with ID: {}", id);
        Ok(coordinator)
    }

    /// Start the application and all components
    pub async fn run(mut self) -> Result<()> {
        let span = span!(Level::INFO, "coordinator_run");
        let _enter = span.enter();

        info!("Starting Pancetta application");
        self.is_running.store(true, Ordering::Release);

        // If WAV playback mode, run the short-circuit pipeline and exit
        if let Some(ref wav_path) = self.wav_path {
            let path = wav_path.clone();
            return self.run_wav_playback(path).await;
        }

        // Initialize metrics if enabled
        if self.enable_metrics {
            self.init_metrics().await?;
        }

        // Start all components in dependency order using point-to-point channels
        self.start_pipeline().await?;

        // Start auxiliary components
        #[cfg(feature = "pancetta-hamlib")]
        self.start_hamlib_component().await?;
        #[cfg(not(feature = "pancetta-hamlib"))]
        warn!("Hamlib feature is disabled -- PTT safety watchdog is not active. Transmit at your own risk.");
        self.start_qso_component().await?;

        // Initialize cqdx.io integration (before autonomous, so rarity/needed data is available)
        {
            let config = self.config.read().await;
            if let Some(bridge) = crate::cqdx_bridge::CqdxBridge::from_config(
                &config.network.cqdx,
                self.cached_lookup.clone(),
            )
            .map(|b| b.with_operating_frequency(self.operating_frequency_hz.clone()))
            {
                drop(config);
                match bridge.startup().await {
                    Ok(()) => {
                        info!("cqdx.io integration initialized");
                        let poller_handle = bridge.spawn_spot_poller(
                            self.shutdown_signal.clone(),
                            self.last_decode_timestamp.clone(),
                            None,
                            None, // TUI tx — set up later in pipeline if available
                        );
                        // Wrap the JoinHandle<()> into JoinHandle<Result<()>> for named_task_handles
                        let wrapped = tokio::spawn(async move {
                            poller_handle
                                .await
                                .map_err(|e| anyhow::anyhow!("cqdx poller join error: {}", e))?;
                            Ok(())
                        });
                        self.named_task_handles
                            .push((ComponentId::DxCluster, wrapped));
                        self.cqdx_bridge = Some(std::sync::Arc::new(bridge));
                    }
                    Err(e) => {
                        warn!("cqdx.io startup failed, running in degraded mode: {}", e);
                    }
                }
            } else {
                drop(config);
                info!("cqdx.io integration not configured, running in degraded mode");
            }
        }

        // hb-062 + Phase-5 hardening #1: build production FP filter.
        // Sources:
        //   1. ~/.pancetta/qsos.adi (operator log)
        //   2. ~/.pancetta/callsign_seed.txt (operator-curated seed list)
        //   3. cqdx-spotted callsigns (refreshed periodically from cqdx_bridge)
        //   4. rolling window populated by accepted decodes this session
        // Cold-start lenient: accept all decodes until reference size
        // reaches `COLD_START_THRESHOLD` (5). The 2026-05-30 live capture
        // showed the previous threshold of 100 left the filter dormant
        // the entire session — empty ADIF + no cqdx config meant
        // reference_size stayed at 0 for 149 minutes and ~3.4k
        // OSD-fabricated decodes leaked through. A small seed file is
        // now enough to flip into strict mode immediately.
        const COLD_START_THRESHOLD: usize = 5;
        {
            let adif_path = dirs::home_dir().map(|h| h.join(".pancetta").join("qsos.adi"));
            let seed_path = dirs::home_dir().map(|h| h.join(".pancetta").join("callsign_seed.txt"));
            let adif_count = adif_path
                .as_ref()
                .filter(|p| p.exists())
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|t| pancetta_qso::callsign_continuity::parse_adif_calls(&t).len())
                .unwrap_or(0);
            let seed: Vec<String> = seed_path
                .as_ref()
                .and_then(|p| {
                    pancetta_qso::callsign_continuity::parse_seed_file(p)
                        .map_err(|e| {
                            warn!("FP filter: failed to read seed file {:?}: {}", p, e);
                            e
                        })
                        .ok()
                })
                .unwrap_or_default();
            let seed_count = seed.len();
            let initial_cqdx_spotted: std::collections::HashSet<String> =
                if let Some(ref bridge) = self.cqdx_bridge {
                    let cache = bridge.cache();
                    let guard = cache.read().await;
                    guard.spotted_callsigns()
                } else {
                    std::collections::HashSet::new()
                };
            let cqdx_count = initial_cqdx_spotted.len();
            match pancetta_qso::callsign_continuity::build_filter_with_seed(
                adif_path.as_deref(),
                initial_cqdx_spotted,
                seed,
                500, // rolling-window capacity
                COLD_START_THRESHOLD,
            ) {
                Ok(filter) => {
                    let total_unique = filter.reference_size();
                    info!(
                        target: "fp_filter",
                        "FP filter sources: adif={} cqdx={} seed={} total_unique={} cold_start_threshold={}",
                        adif_count, cqdx_count, seed_count, total_unique, COLD_START_THRESHOLD
                    );
                    if total_unique < COLD_START_THRESHOLD {
                        warn!(
                            target: "fp_filter",
                            "FP filter reference set is small ({}/{}); decodes will pass unfiltered \
                             until rolling window populates. Populate {} or configure cqdx for \
                             better coverage.",
                            total_unique,
                            COLD_START_THRESHOLD,
                            seed_path
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "~/.pancetta/callsign_seed.txt".to_string())
                        );
                    }
                    self.fp_filter = Some(std::sync::Arc::new(filter));
                }
                Err(e) => {
                    warn!("FP filter init failed, decodes will pass unfiltered: {}", e);
                }
            }
        }

        // Phase-5 hardening #2: seed the priority engine's
        // "excluded DXCC prefixes" set from operator config + ADIF.
        // Used by `CachedStationLookup::is_needed_dxcc` when cqdx
        // hasn't populated a needed-set. Without this, the empty-set
        // fallback returns true for every callsign — inflating CQ
        // scores so the operator would consider every station "needed"
        // (or, under different weighting, treat none as needed). This
        // gives the autonomous operator a defensible signal: anything
        // outside the operator's home DXCC + already-worked DXCCs
        // counts as needed.
        {
            let config = self.config.read().await;
            let operator_callsign = config.station.callsign.clone();
            let dxcc_entity = config.station.dxcc_entity;
            drop(config);
            let adif_path = dirs::home_dir().map(|h| h.join(".pancetta").join("qsos.adi"));
            let excluded = crate::priority_evaluator::default_excluded_dxcc_prefixes(
                &operator_callsign,
                dxcc_entity,
                adif_path.as_deref(),
            );
            let n = excluded.len();
            self.cached_lookup.set_excluded_dxcc_prefixes(excluded);
            info!(
                target: "priority",
                "needed_dxcc default: excluded {} prefixes (home={} entity={}); \
                 cqdx-populated needed-set will override when available",
                n, operator_callsign, dxcc_entity
            );
        }

        self.start_transmitter_component().await?;

        // If --test-tx was passed, inject a single TransmitRequest after a
        // brief settle period, then trigger shutdown after a generous window
        // covering the worst-case TX cycle (slot wait + 12.64s TX + tail).
        if let Some(test_tx_text) = self.test_tx.clone() {
            let bus = self.message_bus.clone();
            let shutdown = self.shutdown_signal.clone();
            let offset = self.test_tx_offset;
            tokio::spawn(async move {
                // Settle: let hamlib spawn rigctld and connect.
                tokio::time::sleep(Duration::from_secs(3)).await;

                info!(
                    "TEST-TX: injecting TransmitRequest '{}' at offset {:.0} Hz",
                    test_tx_text, offset
                );

                let req = crate::message_bus::ComponentMessage::new(
                    crate::message_bus::ComponentId::Coordinator,
                    crate::message_bus::ComponentId::Ft8Transmitter,
                    crate::message_bus::MessageType::TransmitRequest {
                        message_text: test_tx_text.clone(),
                        frequency_offset: offset,
                        qso_id: None,
                        tx_parity: None, // test-TX injection: no DX context
                        origin: crate::message_bus::TxOrigin::Local,
                    },
                    Instant::now(),
                );
                if let Err(e) = bus.send_message(req).await {
                    error!("TEST-TX: send TransmitRequest failed: {}", e);
                    shutdown.store(true, Ordering::Release);
                    return;
                }

                // Worst case: ≤16s slot wait + 12.64s TX + tail/settle = ~30s.
                tokio::time::sleep(Duration::from_secs(35)).await;
                info!("TEST-TX: cycle window elapsed — shutting down");
                shutdown.store(true, Ordering::Release);
            });
        }

        self.start_autonomous_component().await?;
        self.start_dx_cluster_component().await?;
        self.start_pskreporter_component().await?;
        self.start_remote_gateway_component().await?;
        self.start_station_agent_component().await?;

        // Start coordinator tasks
        self.start_coordinator_tasks().await?;

        let startup_duration = self.startup_time.elapsed();
        info!(
            "Application startup completed in {:.2}s",
            startup_duration.as_secs_f64()
        );

        // Main application loop
        self.run_main_loop().await?;

        // Graceful shutdown
        self.shutdown().await?;

        Ok(())
    }

    /// Initialize metrics collection
    async fn init_metrics(&self) -> Result<()> {
        info!("Initializing metrics on port {}", self.metrics_port);

        #[cfg(feature = "prometheus")]
        {
            // `.context()` on the exporter's `Result<(), BuildError>` needs the
            // anyhow extension trait in scope (only `anyhow::Result` is imported
            // at module level). BuildError: std::error::Error, so Context applies.
            use anyhow::Context as _;
            use metrics_exporter_prometheus::PrometheusBuilder;

            let builder =
                PrometheusBuilder::new().with_http_listener(([0, 0, 0, 0], self.metrics_port));

            builder
                .install()
                .context("Failed to install Prometheus metrics exporter")?;

            info!("Metrics server started on port {}", self.metrics_port);
        }

        Ok(())
    }

    /// Shared split-TX dial atomic (0 = simplex). Written by the TUI SetSplit
    /// relay, read by the QSO RF stamp.
    pub(crate) fn split_tx_frequency_hz(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.split_tx_frequency_hz.clone()
    }

    /// Operator-held manual TX audio offset atomic in Hz (0 = unset / auto).
    /// Written by the TUI 'o' set-offset modal; read by the manual-call handler
    /// in [`start_qso_component`] to place our TX at a held offset instead of
    /// defaulting to the DX's audio frequency.
    pub(crate) fn tx_offset_hold_hz(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.tx_offset_hold_hz.clone()
    }

    /// Active protocol slot-length atomic in nanoseconds (FT8 → 15e9, FT4 →
    /// 7.5e9), derived once at startup from `[rig].mode`. Cloned into the
    /// decode loop's parity-stamping sites (`SlotParity::of_with_period`).
    pub(crate) fn active_slot_ns(&self) -> Arc<std::sync::atomic::AtomicI64> {
        self.active_slot_ns.clone()
    }

    /// Station-agent remote-TX arm gate (shared handle). Cloned into the TX
    /// worker, which checks `tx_permitted(now_ms)` before keying PTT for any
    /// `TxOrigin::Remote` request and drops it (fail-closed) if not permitted.
    /// In P0–P2 nothing arms it and no remote request is constructed, so it is
    /// inert; local TX never consults it.
    pub(crate) fn remote_tx_arm(&self) -> Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>> {
        self.remote_tx_arm.clone()
    }

    /// Active digital-mode protocol (FT8 / FT4 / FT2), derived once at startup
    /// from `[rig].mode`. Captured into the TX worker so its encode+modulate
    /// emits the correct on-air waveform for the mode (`Ft8` = legacy path,
    /// byte-identical). `Copy`.
    pub(crate) fn active_protocol(&self) -> pancetta_ft8::Protocol {
        self.active_protocol
    }

    /// Fox-mode activation flag. `false` by default (normal Hound / station
    /// operation). Toggled by [`QsoMessage::SetFoxMode`]; the QSO component
    /// reads it to switch to Fox-mode QSO sequencing and raise the answer cap.
    pub(crate) fn fox_mode(&self) -> Arc<AtomicBool> {
        self.fox_mode.clone()
    }

    /// Maximum simultaneous caller-answer QSOs while Fox mode is engaged.
    /// Seeded from `config.fox.max_streams` at construction. Shared into the
    /// QSO component task so the `maybe_answer_caller` cap can switch without
    /// re-reading config.
    pub(crate) fn fox_max_streams(&self) -> Arc<AtomicUsize> {
        self.fox_max_streams.clone()
    }
}

#[cfg(test)]
mod tx_active_set_tests {
    use super::{active_tx_qso_key, tx_qso_is_live};
    use std::collections::HashSet;

    /// Keys are normalized (trimmed + uppercased) so producer/consumer agree.
    #[test]
    fn key_normalizes_case_and_whitespace() {
        assert_eq!(active_tx_qso_key("abc-123"), "ABC-123");
        assert_eq!(active_tx_qso_key("  AbC-123 "), "ABC-123");
    }

    /// A request with no qso_id (manual free-text / tune / test-TX) is never
    /// gated — always live.
    #[test]
    fn no_qso_id_is_always_live() {
        let empty = HashSet::new();
        assert!(tx_qso_is_live(None, &empty));
    }

    /// A request whose QSO is in the active set is allowed; one whose QSO is
    /// absent (superseded / cancelled / completed-past-grace) is dropped.
    #[test]
    fn membership_decides_live_vs_drop() {
        let mut active = HashSet::new();
        active.insert(active_tx_qso_key("qso-live"));

        // Live QSO → transmit (case-insensitive match).
        assert!(tx_qso_is_live(Some("qso-live"), &active));
        assert!(tx_qso_is_live(Some("QSO-LIVE"), &active));

        // Ended QSO not in the set → drop.
        assert!(!tx_qso_is_live(Some("qso-ended"), &active));
    }

    /// Simulate the superseded/cancelled case: the QSO id is removed from the
    /// set (as the QSO component does on terminal-Failed), and its queued TX
    /// is then dropped at key-time.
    #[test]
    fn superseded_qso_removed_then_dropped() {
        let mut active = HashSet::new();
        let a = active_tx_qso_key("qso-a");
        let b = active_tx_qso_key("qso-b");
        active.insert(a.clone());
        active.insert(b.clone());

        // qso-a is superseded → removed immediately.
        active.remove(&a);

        // qso-a's still-queued frame is now dropped; qso-b keeps transmitting.
        assert!(!tx_qso_is_live(Some("qso-a"), &active));
        assert!(tx_qso_is_live(Some("qso-b"), &active));
    }

    /// Completion grace: while the id is still present (within the ~16s grace),
    /// the final 73 is allowed; after the grace removes it, leftover backlog is
    /// dropped.
    #[test]
    fn completed_qso_grace_then_drop() {
        let mut active = HashSet::new();
        let c = active_tx_qso_key("qso-c");
        active.insert(c.clone());

        // During grace: final 73 still goes out.
        assert!(tx_qso_is_live(Some("qso-c"), &active));

        // Grace elapsed (delayed task removed it): backlog dropped.
        active.remove(&c);
        assert!(!tx_qso_is_live(Some("qso-c"), &active));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pancetta_config::Config;

    // ------------------------------------------------------------------
    // DspTiming derivation — FT8 byte-identical regression guard + FT4.
    // ------------------------------------------------------------------

    #[test]
    fn derive_dsp_timing_ft8_byte_identical() {
        // HARD REGRESSION INVARIANT: FT8 must yield exactly today's values.
        let t = derive_dsp_timing(&pancetta_ft8::ProtocolParams::ft8());
        // window_seconds = 79 * 0.16 = 12.64 → 12.64 * 12000 = 151_680.
        assert_eq!(t.window_samples, 151_680);
        // decode_phase = 15.0 - 2.0 = 13.0s.
        assert_eq!(t.decode_phase, chrono::Duration::seconds(13));
        // overlap = 15.0 * 12000 = 180_000 (= FT8_SAMPLE_RATE * 15).
        assert_eq!(t.overlap_samples, FT8_SAMPLE_RATE * 15);
        assert_eq!(t.overlap_samples, 180_000);
        // slot_ns = 15e9.
        assert_eq!(t.slot_ns, 15_000_000_000);
    }

    #[test]
    fn derive_dsp_timing_ft4() {
        let t = derive_dsp_timing(&pancetta_ft8::ProtocolParams::ft4());
        // window_seconds = 105 * 0.048 = 5.04 → 5.04 * 12000 = 60_480.
        assert_eq!(t.window_samples, 60_480);
        // decode_phase = 7.5 - 1.0 = 6.5s = 6_500ms.
        assert_eq!(t.decode_phase, chrono::Duration::milliseconds(6_500));
        // overlap = 7.5 * 12000 = 90_000 (= FT8_SAMPLE_RATE * 7.5).
        assert_eq!(t.overlap_samples, 90_000);
        // slot_ns = 7.5e9.
        assert_eq!(t.slot_ns, 7_500_000_000);
    }

    // ------------------------------------------------------------------
    // Cross-seam composition: derive_dsp_timing + Protocol::slot_ns +
    // the slot.rs `_with_period` helpers must compose into one
    // self-consistent grid (7.5s for FT4, 15s for FT8). This is the
    // end-to-end "FT8 unchanged + FT4 7.5s grid" regression guard the
    // FT4 plan (Task 8) calls for. It replicates the EXACT production
    // parity-stamping seam in `coordinator/ft8.rs`:
    //     slot_start = window_received_utc - decode_phase;
    //     parity     = SlotParity::of_with_period(slot_start, slot_ns);
    // so a window received `decode_phase` past a slot boundary recovers
    // the correct slot start + parity on the protocol's own grid.
    //
    // Wall-clock-free: `now` is a fixed timestamp (the same 2026-01-01
    // 00:00:00 UTC reference used by tx.rs::schedule_tx_tests — its
    // unix timestamp 1767225600 is divisible by both 15 and 7.5, so
    // slot 0 is Even on both grids).
    // ------------------------------------------------------------------

    /// Reference epoch: 2026-01-01 00:00:00 UTC (= 1767225600s), the same
    /// instant tx.rs::schedule_tx_tests uses. Slot 0 is Even on the 15s
    /// AND the 7.5s grid.
    fn epoch_at(seconds: f64) -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        let base = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        base + chrono::Duration::nanoseconds((seconds * 1_000_000_000.0) as i64)
    }

    /// Recover (slot_start, parity) from a window received `decode_phase`
    /// past a slot boundary — the exact arithmetic the FT8 hot loop runs
    /// at `coordinator/ft8.rs:654-656`.
    fn recover_slot(
        window_received: chrono::DateTime<chrono::Utc>,
        t: &DspTiming,
    ) -> (
        chrono::DateTime<chrono::Utc>,
        pancetta_core::slot::SlotParity,
    ) {
        let slot_start = window_received - t.decode_phase;
        let parity = pancetta_core::slot::SlotParity::of_with_period(slot_start, t.slot_ns);
        (slot_start, parity)
    }

    #[test]
    fn ft4_timing_seam_composes_to_7_5s_grid() {
        // FT4: derive timing, then walk several consecutive 7.5s slots and
        // assert the decode-phase recovery lands on the right grid boundary
        // with alternating parity. Slot 0 (:00.0) Even, slot 1 (:07.5) Odd,
        // slot 2 (:15.0) Even, slot 3 (:22.5) Odd.
        let t = derive_dsp_timing(&pancetta_ft8::ProtocolParams::ft4());
        assert_eq!(t.slot_ns, 7_500_000_000);
        // Protocol::slot_ns agrees with the derived value (single source).
        assert_eq!(t.slot_ns, pancetta_ft8::Protocol::Ft4.slot_ns());

        let decode_phase_secs = t.decode_phase.num_nanoseconds().unwrap() as f64 / 1e9;
        assert!((decode_phase_secs - 6.5).abs() < 1e-9);

        // For each slot k, a window received at slot_start + decode_phase
        // must recover slot_start and the parity matching k.
        for k in 0..4 {
            let slot_start_secs = k as f64 * 7.5;
            let window_received = epoch_at(slot_start_secs + decode_phase_secs);
            let (recovered_start, parity) = recover_slot(window_received, &t);
            assert_eq!(
                recovered_start,
                epoch_at(slot_start_secs),
                "FT4 slot {k}: decode_phase recovery must land on the 7.5s boundary"
            );
            let expected = if k % 2 == 0 {
                pancetta_core::slot::SlotParity::Even
            } else {
                pancetta_core::slot::SlotParity::Odd
            };
            assert_eq!(parity, expected, "FT4 slot {k}: parity mismatch");
            // Cross-check against the canonical slot-start helper on the
            // SAME grid (window_received still sits inside slot k).
            assert_eq!(
                pancetta_core::slot::current_slot_start_with_period(window_received, t.slot_ns),
                epoch_at(slot_start_secs),
                "FT4 slot {k}: current_slot_start_with_period disagrees"
            );
        }
    }

    #[test]
    fn ft8_timing_seam_byte_identical_to_legacy_grid() {
        // HARD REGRESSION INVARIANT: FT8 timing composed through the same
        // seam must reproduce today's 15s grid AND match the FT8 wrapper
        // helpers (SlotParity::of / current_slot_start, which hardcode
        // SLOT_NS) exactly. Slot 0 (:00) Even, slot 1 (:15) Odd, etc.
        let t = derive_dsp_timing(&pancetta_ft8::ProtocolParams::ft8());
        assert_eq!(t.slot_ns, pancetta_core::slot::SLOT_NS);
        assert_eq!(t.slot_ns, pancetta_ft8::Protocol::Ft8.slot_ns());
        assert_eq!(t.decode_phase, chrono::Duration::seconds(13));

        for k in 0..4 {
            let slot_start_secs = k as f64 * 15.0;
            let window_received = epoch_at(slot_start_secs + 13.0);
            let (recovered_start, parity) = recover_slot(window_received, &t);
            assert_eq!(recovered_start, epoch_at(slot_start_secs));
            // The period-generic recovery must agree byte-for-byte with the
            // FT8-hardcoded wrapper used before FT4 wiring landed.
            let legacy_parity = pancetta_core::slot::SlotParity::of(recovered_start);
            assert_eq!(
                parity, legacy_parity,
                "FT8 slot {k}: parity drift vs legacy"
            );
            assert_eq!(
                pancetta_core::slot::current_slot_start(window_received),
                epoch_at(slot_start_secs),
                "FT8 slot {k}: current_slot_start drift"
            );
            let expected = if k % 2 == 0 {
                pancetta_core::slot::SlotParity::Even
            } else {
                pancetta_core::slot::SlotParity::Odd
            };
            assert_eq!(parity, expected);
        }
    }

    #[test]
    fn ft4_grid_is_twice_as_dense_as_ft8() {
        // A direct contrast: at the same instant past a boundary, FT8 and
        // FT4 recover DIFFERENT slot starts because the grids differ. At
        // :15.0 exactly, FT8 is at the slot-1 boundary (Odd) while FT4 is
        // at its slot-2 boundary (Even) — proving the 7.5s grid is live,
        // not a relabeled 15s grid.
        let ft8 = derive_dsp_timing(&pancetta_ft8::ProtocolParams::ft8());
        let ft4 = derive_dsp_timing(&pancetta_ft8::ProtocolParams::ft4());
        // window received exactly decode_phase past the :15.0 boundary on
        // each grid.
        let ft8_recv = epoch_at(15.0 + 13.0);
        let ft4_recv = epoch_at(15.0 + 6.5);
        let (ft8_start, ft8_par) = recover_slot(ft8_recv, &ft8);
        let (ft4_start, ft4_par) = recover_slot(ft4_recv, &ft4);
        assert_eq!(ft8_start, epoch_at(15.0));
        assert_eq!(ft4_start, epoch_at(15.0));
        assert_eq!(ft8_par, pancetta_core::slot::SlotParity::Odd);
        assert_eq!(ft4_par, pancetta_core::slot::SlotParity::Even);
        assert_ne!(ft8.slot_ns, ft4.slot_ns);
        assert_eq!(ft8.slot_ns, 2 * ft4.slot_ns);
    }

    #[test]
    fn protocol_from_mode_maps_ft8_and_ft4() {
        assert_eq!(
            protocol_from_mode(pancetta_config::OperatingMode::Ft8),
            pancetta_ft8::Protocol::Ft8
        );
        assert_eq!(
            protocol_from_mode(pancetta_config::OperatingMode::Ft4),
            pancetta_ft8::Protocol::Ft4
        );
    }

    #[test]
    fn mode_str_maps_each_operating_mode() {
        assert_eq!(mode_str(pancetta_config::OperatingMode::Ft8), "FT8");
        assert_eq!(mode_str(pancetta_config::OperatingMode::Ft4), "FT4");
        assert_eq!(mode_str(pancetta_config::OperatingMode::Ft2), "FT2");
    }

    #[tokio::test]
    async fn test_coordinator_creation() {
        let config = Config::default();
        let shutdown = Arc::new(AtomicBool::new(false));

        let coordinator = ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None, // no WAV
            None, // no test-tx
            1500.0,
            shutdown,
            Vec::new(), // no config warnings
        )
        .await;

        assert!(coordinator.is_ok());
    }

    #[tokio::test]
    async fn test_coordinator_config() {
        let config = CoordinatorConfig::default();

        assert_eq!(config.startup_timeout, Duration::from_secs(30));
        assert_eq!(config.shutdown_timeout, Duration::from_secs(10));
        assert!(config.message_buffer_size > 0);
    }

    #[test]
    fn test_resample_identity() {
        let input = vec![1.0, 2.0, 3.0, 4.0];
        let output = resample_linear(&input, 48000, 48000);
        assert_eq!(output.len(), 4);
    }

    #[test]
    fn test_resample_downsample() {
        let input: Vec<f32> = (0..48000).map(|i| (i as f32 / 48000.0).sin()).collect();
        let output = resample_linear(&input, 48000, 12000);
        // Should be approximately 12000 samples
        assert!((output.len() as i64 - 12000).abs() <= 1);
    }

    #[tokio::test]
    async fn test_wav_playback_decodes_messages() {
        // Use a known WAV fixture from the FT8 test suite
        let wav_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../pancetta-ft8/tests/fixtures/wav/wsjt/210703_133430.wav");

        if !wav_path.exists() {
            eprintln!(
                "Skipping WAV playback test: fixture not found at {:?}",
                wav_path
            );
            return;
        }

        let config = Config::default();
        let shutdown = Arc::new(AtomicBool::new(false));

        let coordinator = ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // no metrics
            9090,
            Some(wav_path),
            None, // no test-tx
            1500.0,
            shutdown,
            Vec::new(), // no config warnings
        )
        .await
        .expect("coordinator creation should succeed");

        // run_wav_playback exits after decoding -- should not error
        let result = coordinator.run().await;
        assert!(
            result.is_ok(),
            "WAV playback should succeed: {:?}",
            result.err()
        );
    }
}
