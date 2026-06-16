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
mod shutdown;
mod tier;
mod tui_relay;
mod tx;
mod util;
mod wav_playback;

pub use tx::{
    coalesce_transmit_requests, resolve_required_parity, schedule_tx, CoalesceEntry,
    CoalesceOutcome, TxSchedule,
};

// Re-export the C19 config-reload classifier (safe-live vs deferred) and the
// C20 RF-present/no-decode detector so the coordinator-robustness integration
// tests can exercise the real production decision logic.
pub use health::{classify_config_reload, ConfigReloadApplicability, RfNoDecodeMonitor};

/// Canonical key for the `active_tx_qsos` set: QSO ids are compared
/// case-insensitively (and trimmed) so the producer (QSO component) and
/// consumer (TX worker) never disagree on casing. Centralized here so the
/// insert / remove / membership-test sites can't drift.
pub fn active_tx_qso_key(qso_id: &str) -> String {
    qso_id.trim().to_uppercase()
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

use anyhow::Result;
use pancetta_config::Config;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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

    /// Performance metrics
    message_count: Arc<std::sync::atomic::AtomicU64>,
    last_audio_timestamp: Arc<RwLock<Option<Instant>>>,
    last_decode_timestamp: Arc<RwLock<Option<Instant>>>,

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

        // Wrap config in Arc<RwLock> for hot-reloading
        let config = Arc::new(RwLock::new(config));

        // hb-216 S2: shared FT8 decoder config + scoped-fast-path atomic.
        // `tier::initialize` seeds the atomic from env, reads the on-disk
        // cache if present, and spawns a background probe on cache miss.
        // The FT8 hot loop reads both fields without blocking on probe
        // completion.
        let ft8_config = Arc::new(RwLock::new(Ft8Config::default()));
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
            #[cfg(feature = "pancetta-hamlib")]
            rigctld_process: None,
            message_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_audio_timestamp: Arc::new(RwLock::new(None)),
            last_decode_timestamp: Arc::new(RwLock::new(None)),
            audio_output_default: Arc::new(AtomicBool::new(false)),
            audio_reopen_tx: None,
            rig_conn_state: Arc::new(std::sync::atomic::AtomicU8::new(
                crate::coordinator::hamlib::RigConnState::default().as_u8(),
            )),
            scoped_fast_path,
            ft8_config,
            config_warnings,
            active_tx_qsos: Arc::new(std::sync::RwLock::new(HashSet::new())),
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
            ) {
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
