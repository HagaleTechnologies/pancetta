//! Deterministic QSO simulation harness — a "virtual band + virtual clock"
//! vehicle that drives the **real** [`QsoManager`] (and, optionally, the real
//! [`SmartFrequencyAllocator`]) through complete QSOs with no audio, no rig,
//! and no real-time waiting.
//!
//! # Why this exists
//!
//! Pancetta's QSO engine is the most subtle piece of on-air logic: it sequences
//! a contact (CQ → grid → report → R-report → RR73 → 73), keep-calls under a
//! watchdog, backs up the state ladder when the DX repeats an earlier message,
//! rejects spoofed senders, and never works the same station twice. Validating
//! all of that against a live radio is slow, non-deterministic, and impossible
//! to script for the *weird* cases (a DX that fades, a DX mid-exchange with a
//! third party, a band so crowded our TX offset has to move).
//!
//! This harness lets us script those situations as plain data and assert the
//! engine's behavior slot-by-slot, deterministically, in milliseconds. It is
//! **keepable infrastructure**: the permanent scenario catalog in
//! `tests/qso_scenarios.rs` is built on it, and `examples/qso_sim.rs` is a
//! runnable sandbox for trying new ideas.
//!
//! # What it drives (and what it does *not* reimplement)
//!
//! The harness owns **no QSO logic of its own**. It creates one real
//! [`QsoManager`], [`subscribe`](QsoManager::subscribe)s to its event stream,
//! and provides three things:
//!
//! 1. A **virtual clock** ([`SimClock`]) that advances in 15-second FT8 slots.
//!    The engine already takes an explicit `now` for the time-sensitive paths
//!    ([`check_timeouts_at`](QsoManager::check_timeouts_at) and
//!    [`rearm_manual_calls_at`](QsoManager::rearm_manual_calls_at)); the harness
//!    threads the virtual clock into both so nothing waits on wall-clock time.
//!    The watchdog (5 min / 10 calls) is therefore reachable in a handful of
//!    fast ticks.
//!
//! 2. A **virtual band / ether** ([`Sim::inject_decode`]): you hand it a decoded
//!    message (text, audio offset, SNR, dt, slot parity) and it feeds it to the
//!    engine via [`process_message`](QsoManager::process_message) — parsing the
//!    text with the *real* [`MessageExchange`](crate::exchange::MessageExchange)
//!    parser, so `RR73` / `RRR` / `73` are classified exactly as on-air.
//!    Everything **we** transmit (every [`QsoEvent::MessageToSend`]) is captured
//!    into a [`Transmission`].
//!
//! 3. A per-slot **tick** ([`Sim::tick`]) that, for the current slot: delivers
//!    that slot's injected decodes, runs the engine's manual keep-call re-arm
//!    and timeout watchdog at the slot's virtual `now`, drains all emitted
//!    events, and records our transmissions + state transitions + completions +
//!    failures into a [`Timeline`].
//!
//! # Authoring a scenario
//!
//! Two styles, both backed by the same primitives:
//!
//! - **Imperative** — call [`Sim::call_station`] / [`Sim::cq`] /
//!   [`Sim::respond_to_caller`] / [`Sim::abort`] to drive the operator side, and
//!   [`Sim::inject_decode`] to inject the DX side, then [`Sim::tick`] to advance
//!   a slot. This is what most scenario tests use because it reads like an
//!   on-air log.
//!
//! - **Scripted** — build a [`Scenario`] (a list of `(slot, Vec<SimAction>)`)
//!   and run it with [`Sim::run_scenario`]. Good for declarative, table-style
//!   cases and for the example sandbox.
//!
//! # Band-condition modeling
//!
//! Band conditions manifest, for the engine, at the **decoded-message level** —
//! that is the only band information the QSO engine ever sees. So the harness
//! models them there:
//!
//! - **Fade / weak**: simply *don't* inject the DX's decode for some slots
//!   (the DX "disappeared"), or inject it with a low/variable SNR. The keep-call
//!   re-arm fires on every tick regardless, so a QSO can still complete over
//!   more slots — or, if the DX never comes back, the watchdog retires it.
//!
//! - **Crowded**: inject many unrelated decodes across the passband
//!   ([`Sim::inject_crowd`]) and feed the same activity into a
//!   [`BandModel`] backing the [`SmartFrequencyAllocator`]. Then call
//!   [`Sim::choose_tx_offset`] to exercise the allocator and assert the chosen
//!   offset avoids the occupied region (or shifts when our spot is taken).
//!
//! - **Collision**: inject two decodes at ~the same offset in one slot
//!   ([`Sim::inject_collision`]) — useful for asserting frequency-tolerance and
//!   relevance behavior.
//!
//! # Asserting the outcome
//!
//! Drive the scenario, then call assertion helpers on the [`Timeline`]:
//! [`Timeline::assert_completed_with`], [`Timeline::assert_transmitted_contains`],
//! [`Timeline::assert_no_duplicate_qsos`], [`Timeline::assert_failed_with`], and
//! the allocator helper [`assert_tx_offset_clear_of`]. The [`Timeline`] also
//! `Display`s as a readable `slot | RX | TX | state` table for debugging and for
//! the example vehicle.
//!
//! # A note on time
//!
//! The engine stamps the *message-received* timestamp inside
//! [`process_message`](QsoManager::process_message) with the real wall clock,
//! which the harness cannot intercept. That does **not** affect any timeline
//! outcome: message routing and state transitions are time-independent, and the
//! only time-driven behavior — the manual keep-call watchdog and the per-state
//! timeouts — is governed entirely by the explicit virtual `now` the harness
//! passes to [`check_timeouts_at`](QsoManager::check_timeouts_at) and
//! [`rearm_manual_calls_at`](QsoManager::rearm_manual_calls_at). The virtual
//! clock is the single source of truth for everything that matters.

use std::collections::BTreeSet;
use std::fmt;

use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use pancetta_core::slot::SlotParity;
use pancetta_core::ResponseStep;
use tokio::sync::broadcast::error::TryRecvError;

use crate::exchange::MessageExchange;
use crate::frequency::{
    DecodeHistory, DecodeRecord, FrequencyAllocatorConfig, SmartFrequencyAllocator,
    SpectralSnapshot, TimeSlot,
};
use crate::qso_manager::{QsoEvent, QsoManager, QsoManagerConfig};
use crate::states::{MessageType, QsoFailureReason, QsoState};

/// Length of one FT8 slot, in seconds.
pub const SLOT_SECONDS: i64 = 15;

/// A deterministic virtual clock advancing in 15-second FT8 slots.
///
/// Slot 0 starts at the **15s slot boundary containing the wall clock when the
/// harness is built** ([`SimClock::base`]) — i.e. real `now` snapped *down* to
/// the current slot start. This anchoring is deliberate and load-bearing.
///
/// The engine stamps `QsoState::started_at` / `first_call_at` with the real
/// `Utc::now()`, and the per-state timeouts / manual watchdog compute
/// `virtual_now - started_at` (then cast to `u64` for comparison). Two failure
/// modes must be avoided:
///
/// - If the virtual base were in the **past** of those real timestamps, the
///   difference would be negative and the `as u64` cast would wrap to a huge
///   value, spuriously timing out *every* QSO immediately.
/// - If the virtual base were far in the **future** of real now, slot 0 would
///   already show a large elapsed time, again risking a spurious per-state
///   timeout on the very first slot.
///
/// Snapping *down* to the containing slot boundary puts `base` at most one slot
/// (<15 s) before real `now`, so any `started_at` (captured during operator
/// actions, essentially at real `now`) lands in `[base, base + 15 s)`. Elapsed
/// time therefore starts in `[0, 15 s)` and grows by exactly one slot per
/// [`SimClock::advance`]. The 30 s per-state timeouts fire ~2 slots after a
/// state is entered (realistic FT8 behavior, and the harness advances the QSO
/// out of those states with the next injected decode first), and the 5-minute /
/// 10-call manual watchdog is reachable and deterministic. Slot parity is taken
/// from the base boundary via [`SlotParity::of`], so it is internally consistent
/// with the engine's own parity math.
#[derive(Debug, Clone)]
pub struct SimClock {
    base: DateTime<Utc>,
    slot: u64,
}

impl SimClock {
    /// A fixed reference instant (2026-01-01T00:00:00Z), retained for callers
    /// that want a stable, parity-defined origin in unit tests via
    /// [`SimClock::with_base`]. Not used as the live base — see the [`SimClock`]
    /// type docs for why the live base anchors to wall-clock now.
    pub fn epoch() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("fixed sim epoch 2026-01-01T00:00:00Z is a valid, unambiguous UTC instant")
    }

    /// Snap an instant *down* to the 15s slot boundary containing it.
    fn slot_floor(t: DateTime<Utc>) -> DateTime<Utc> {
        let ns = t
            .timestamp_nanos_opt()
            .expect("wall clock within representable ns range");
        const SLOT_NS: i64 = SLOT_SECONDS * 1_000_000_000;
        let idx = ns.div_euclid(SLOT_NS);
        DateTime::<Utc>::from_timestamp_nanos(idx * SLOT_NS)
    }

    /// Create a clock at slot 0, anchored to the slot boundary containing the
    /// current wall clock.
    pub fn new() -> Self {
        Self {
            base: Self::slot_floor(Utc::now()),
            slot: 0,
        }
    }

    /// Create a clock anchored to an explicit base instant (snapped down to its
    /// containing slot boundary). Mainly for deterministic unit tests.
    pub fn with_base(base: DateTime<Utc>) -> Self {
        Self {
            base: Self::slot_floor(base),
            slot: 0,
        }
    }

    /// The slot-0 base instant (a 15s slot boundary).
    pub fn base(&self) -> DateTime<Utc> {
        self.base
    }

    /// The current slot index (0-based).
    pub fn slot(&self) -> u64 {
        self.slot
    }

    /// The virtual `now` at the start of the current slot.
    pub fn now(&self) -> DateTime<Utc> {
        self.base + ChronoDuration::seconds(self.slot as i64 * SLOT_SECONDS)
    }

    /// The parity of the current slot, derived from the virtual `now` via the
    /// engine's own [`SlotParity::of`] — internally consistent with the slot
    /// math the QSO engine and TX scheduler use.
    pub fn parity(&self) -> SlotParity {
        SlotParity::of(self.now())
    }

    /// Advance to the next slot.
    pub fn advance(&mut self) {
        self.slot += 1;
    }
}

impl Default for SimClock {
    fn default() -> Self {
        Self::new()
    }
}

/// A message **we** transmitted, captured from a [`QsoEvent::MessageToSend`].
#[derive(Debug, Clone)]
pub struct Transmission {
    /// Slot index in which we emitted it.
    pub slot: u64,
    /// The rendered FT8 text (e.g. `"VB7F K5ARH 73"`).
    pub text: String,
    /// The structured message type.
    pub message: MessageType,
    /// Audio offset we transmitted on, in Hz.
    pub freq_hz: f64,
    /// The latched TX parity for the QSO this transmission belongs to.
    pub tx_parity: Option<SlotParity>,
}

/// A message the virtual band delivered to the engine (what we "received").
#[derive(Debug, Clone)]
pub struct Reception {
    /// Slot index in which it was injected.
    pub slot: u64,
    /// The raw decoded text.
    pub text: String,
    /// Audio offset, in Hz.
    pub freq_hz: f64,
    /// Signal-to-noise ratio, in dB.
    pub snr_db: f32,
    /// Time offset of the decode, in seconds (the FT8 "dt").
    pub dt: f32,
    /// The slot parity reported for the decode.
    pub slot_parity: SlotParity,
}

/// A QSO completion observed on the event stream.
#[derive(Debug, Clone)]
pub struct Completion {
    /// Slot index in which the completion was observed.
    pub slot: u64,
    /// The worked station's callsign, if known.
    pub their_callsign: Option<String>,
    /// String form of the QSO id (stable across the run).
    pub qso_id: String,
}

/// A QSO failure observed on the event stream.
#[derive(Debug, Clone)]
pub struct Failure {
    /// Slot index in which the failure was observed.
    pub slot: u64,
    /// Why the QSO failed.
    pub reason: QsoFailureReason,
    /// The station's callsign, if known.
    pub their_callsign: Option<String>,
    /// String form of the QSO id.
    pub qso_id: String,
}

/// One state transition observed on the event stream.
#[derive(Debug, Clone)]
pub struct Transition {
    /// Slot index in which it was observed.
    pub slot: u64,
    /// String form of the QSO id.
    pub qso_id: String,
    /// State before.
    pub old_state: QsoState,
    /// State after.
    pub new_state: QsoState,
}

/// The recorded history of a simulation run.
///
/// Everything that happened, slot by slot, with assertion helpers. This is the
/// object scenario tests inspect. It also `Display`s as a human-readable
/// `slot | RX | TX | state` table.
#[derive(Debug, Default, Clone)]
pub struct Timeline {
    /// Every decode injected, in order.
    pub receptions: Vec<Reception>,
    /// Every message we transmitted, in order.
    pub transmissions: Vec<Transmission>,
    /// Every state transition, in order.
    pub transitions: Vec<Transition>,
    /// Every completion, in order.
    pub completions: Vec<Completion>,
    /// Every failure, in order.
    pub failures: Vec<Failure>,
    /// The highest slot index reached.
    pub last_slot: u64,
}

impl Timeline {
    /// All distinct QSO ids that ever appeared in any event.
    pub fn distinct_qso_ids(&self) -> BTreeSet<String> {
        let mut s = BTreeSet::new();
        for t in &self.transitions {
            s.insert(t.qso_id.clone());
        }
        for c in &self.completions {
            s.insert(c.qso_id.clone());
        }
        for f in &self.failures {
            s.insert(f.qso_id.clone());
        }
        s
    }

    /// Did we transmit any message whose rendered text contains `needle`?
    pub fn transmitted_contains(&self, needle: &str) -> bool {
        self.transmissions.iter().any(|t| t.text.contains(needle))
    }

    /// Count of transmissions whose text contains `needle`.
    pub fn count_transmitted_containing(&self, needle: &str) -> usize {
        self.transmissions
            .iter()
            .filter(|t| t.text.contains(needle))
            .count()
    }

    /// Did a completion with the given callsign occur?
    pub fn completed_with(&self, callsign: &str) -> bool {
        self.completions
            .iter()
            .any(|c| c.their_callsign.as_deref() == Some(callsign))
    }

    /// Did any QSO fail with the given reason?
    pub fn failed_with_reason(&self, reason: &QsoFailureReason) -> bool {
        self.failures.iter().any(|f| &f.reason == reason)
    }

    // --- Assertion helpers (panic with a readable timeline on failure) ---

    /// Assert a QSO completed with the given station.
    pub fn assert_completed_with(&self, callsign: &str) {
        assert!(
            self.completed_with(callsign),
            "expected a completed QSO with {callsign}, but none was observed.\n{self}"
        );
    }

    /// Assert no QSO completed with the given station.
    pub fn assert_not_completed_with(&self, callsign: &str) {
        assert!(
            !self.completed_with(callsign),
            "expected NO completed QSO with {callsign}, but one was observed.\n{self}"
        );
    }

    /// Assert we transmitted a message whose text contains `needle`.
    pub fn assert_transmitted_contains(&self, needle: &str) {
        assert!(
            self.transmitted_contains(needle),
            "expected to have transmitted a message containing {needle:?}, but did not.\n{self}"
        );
    }

    /// Assert we never transmitted a message whose text contains `needle`.
    pub fn assert_not_transmitted_contains(&self, needle: &str) {
        assert!(
            !self.transmitted_contains(needle),
            "expected NEVER to transmit a message containing {needle:?}, but did.\n{self}"
        );
    }

    /// Assert no more than `max` distinct QSO ids ever appeared. The canonical
    /// guard against "a re-call spawned a duplicate QSO".
    pub fn assert_at_most_qsos(&self, max: usize) {
        let ids = self.distinct_qso_ids();
        assert!(
            ids.len() <= max,
            "expected at most {max} distinct QSO(s), saw {}: {ids:?}\n{self}",
            ids.len()
        );
    }

    /// Assert exactly one distinct QSO id appeared across the whole run. Used by
    /// the re-call / double-Space scenarios.
    pub fn assert_no_duplicate_qsos(&self) {
        self.assert_at_most_qsos(1);
    }

    /// Assert a QSO failed with the given reason.
    pub fn assert_failed_with(&self, reason: QsoFailureReason) {
        assert!(
            self.failed_with_reason(&reason),
            "expected a QSO failure with reason {reason:?}, but none was observed.\n{self}"
        );
    }

    /// Assert no `Superseded` failure occurred (a duplicate-QSO smell).
    pub fn assert_no_superseded(&self) {
        assert!(
            !self.failed_with_reason(&QsoFailureReason::Superseded),
            "expected NO Superseded failure, but one was observed.\n{self}"
        );
    }
}

impl fmt::Display for Timeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== QSO Timeline (slots 0..={}) ===", self.last_slot)?;
        writeln!(
            f,
            "{:>4} | {:<28} | {:<28} | events",
            "slot", "RX (band -> us)", "TX (us -> band)"
        )?;
        writeln!(f, "{}", "-".repeat(100))?;
        for slot in 0..=self.last_slot {
            let rx: Vec<String> = self
                .receptions
                .iter()
                .filter(|r| r.slot == slot)
                .map(|r| format!("{} ({:.0}Hz {:+.0}dB)", r.text, r.freq_hz, r.snr_db))
                .collect();
            let tx: Vec<String> = self
                .transmissions
                .iter()
                .filter(|t| t.slot == slot)
                .map(|t| format!("{} ({:.0}Hz)", t.text, t.freq_hz))
                .collect();
            let mut events: Vec<String> = Vec::new();
            for tr in self.transitions.iter().filter(|t| t.slot == slot) {
                events.push(format!(
                    "{}->{}",
                    short_state(&tr.old_state),
                    short_state(&tr.new_state)
                ));
            }
            for c in self.completions.iter().filter(|c| c.slot == slot) {
                events.push(format!(
                    "COMPLETED({})",
                    c.their_callsign.as_deref().unwrap_or("?")
                ));
            }
            for fa in self.failures.iter().filter(|fa| fa.slot == slot) {
                events.push(format!(
                    "FAILED({:?},{})",
                    fa.reason,
                    fa.their_callsign.as_deref().unwrap_or("?")
                ));
            }

            // One row per slot, possibly multi-line if many entries.
            let rows = rx.len().max(tx.len()).max(events.len()).max(1);
            for i in 0..rows {
                let s = if i == 0 {
                    format!("{slot:>4}")
                } else {
                    "    ".to_string()
                };
                writeln!(
                    f,
                    "{} | {:<28} | {:<28} | {}",
                    s,
                    rx.get(i).map(String::as_str).unwrap_or(""),
                    tx.get(i).map(String::as_str).unwrap_or(""),
                    events.get(i).map(String::as_str).unwrap_or("")
                )?;
            }
        }
        Ok(())
    }
}

/// A compact one-word label for a [`QsoState`], for the timeline display.
fn short_state(s: &QsoState) -> &'static str {
    match s {
        QsoState::Idle => "Idle",
        QsoState::CallingCq { .. } => "CallingCq",
        QsoState::RespondingToCq { .. } => "RespondCq",
        QsoState::WaitingForReport { .. } => "WaitReport",
        QsoState::SendingReport { .. } => "SendReport",
        QsoState::WaitingForConfirmation { .. } => "WaitConfirm",
        QsoState::SendingConfirmation { .. } => "SendConfirm",
        QsoState::Completed { .. } => "Completed",
        QsoState::Failed { .. } => "Failed",
        QsoState::Contest(_) => "Contest",
    }
}

/// A virtual band/ether model backing the [`SmartFrequencyAllocator`].
///
/// Holds the spectral snapshot and rolling decode history the allocator scores
/// against. Crowd/collision injections update this so [`Sim::choose_tx_offset`]
/// reflects current conditions. Defaults match the production passband
/// (200–2800 Hz audio offset).
#[derive(Debug, Clone)]
pub struct BandModel {
    /// Power per spectral bin (linear, 0.0–1.0), 200–2800 Hz.
    pub spectral: SpectralSnapshot,
    /// Rolling decode-activity history.
    pub history: DecodeHistory,
}

impl BandModel {
    /// A clean band: flat-zero spectrum, empty history.
    pub fn clear() -> Self {
        Self {
            spectral: SpectralSnapshot {
                power_bins: vec![0.0f32; 140],
                freq_min_hz: 200.0,
                freq_max_hz: 2800.0,
            },
            history: DecodeHistory::new(4),
        }
    }

    /// Mark a contiguous region `center ± radius` as occupied: raise its
    /// spectral power and add decode activity in both time slots so the
    /// allocator scores it as busy.
    pub fn occupy(&mut self, center_hz: f64, radius_hz: f64, power: f32) {
        let bins = self.spectral.power_bins.len();
        if bins > 0 {
            let bin_width = (self.spectral.freq_max_hz - self.spectral.freq_min_hz) / bins as f64;
            if bin_width > 0.0 {
                let lo = (((center_hz - radius_hz) - self.spectral.freq_min_hz) / bin_width)
                    .floor()
                    .max(0.0) as usize;
                let hi = (((center_hz + radius_hz) - self.spectral.freq_min_hz) / bin_width)
                    .ceil()
                    .max(0.0) as usize;
                for i in lo..=hi.min(bins - 1) {
                    self.spectral.power_bins[i] = power;
                }
            }
        }
        self.history.push_cycle(vec![
            DecodeRecord {
                frequency_hz: center_hz,
                time_slot: TimeSlot::First,
            },
            DecodeRecord {
                frequency_hz: center_hz,
                time_slot: TimeSlot::Second,
            },
        ]);
    }
}

impl Default for BandModel {
    fn default() -> Self {
        Self::clear()
    }
}

/// A declarative action in a [`Scenario`] script.
#[derive(Debug, Clone)]
pub enum SimAction {
    /// Operator initiates a manual call to a station calling CQ.
    CallStation {
        /// DX callsign to call.
        callsign: String,
        /// Audio offset to call on, in Hz.
        freq_hz: f64,
    },
    /// Operator initiates a manual CQ.
    Cq {
        /// Audio offset to CQ on, in Hz.
        freq_hz: f64,
    },
    /// Operator replies to a station calling us, starting at the given ladder step.
    RespondToCaller {
        /// Caller's callsign.
        callsign: String,
        /// Audio offset, in Hz.
        freq_hz: f64,
        /// Ladder rung to open at.
        step: ResponseStep,
        /// Our SNR measurement of them, if any.
        our_snr_of_them: Option<f32>,
        /// The report they sent us, if known.
        their_report: Option<i8>,
    },
    /// Operator aborts (cancels) the active QSO with a station.
    Abort {
        /// Callsign to abort.
        callsign: String,
    },
    /// The virtual band delivers a decoded message to the engine.
    Inject {
        /// Decoded text.
        text: String,
        /// Audio offset, in Hz.
        freq_hz: f64,
        /// SNR, in dB.
        snr_db: f32,
        /// Decode dt, in seconds.
        dt: f32,
    },
}

/// A declarative scenario: a list of `(slot, actions)` to run, in slot order.
///
/// Actions scheduled for slot N fire during that slot's [`Sim::tick`]. Use
/// [`Sim::run_scenario`] to execute. The imperative API ([`Sim::call_station`]
/// etc.) is equivalent and often more readable for tests; the scripted form is
/// handy for table-style cases and the example sandbox.
#[derive(Debug, Default, Clone)]
pub struct Scenario {
    name: String,
    steps: Vec<(u64, Vec<SimAction>)>,
}

impl Scenario {
    /// Create a named, empty scenario.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            steps: Vec::new(),
        }
    }

    /// The scenario's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Schedule a batch of actions in slot `slot`.
    pub fn at(mut self, slot: u64, actions: Vec<SimAction>) -> Self {
        self.steps.push((slot, actions));
        self
    }

    /// The highest slot index referenced.
    fn max_slot(&self) -> u64 {
        self.steps.iter().map(|(s, _)| *s).max().unwrap_or(0)
    }
}

/// The simulation harness. Wraps one real [`QsoManager`] + a virtual clock +
/// a virtual band, accumulating a [`Timeline`] as you drive it.
pub struct Sim {
    manager: QsoManager,
    events: tokio::sync::broadcast::Receiver<QsoEvent>,
    exchange: MessageExchange,
    our_callsign: String,
    clock: SimClock,
    /// Decodes injected for the *current* slot, drained on `tick`.
    pending_injects: Vec<Reception>,
    /// The accumulated timeline.
    timeline: Timeline,
    /// Virtual band model for the allocator.
    band: BandModel,
    allocator: SmartFrequencyAllocator,
}

impl Sim {
    /// Build a harness for the given station. Starts the underlying
    /// [`QsoManager`] and subscribes to its event stream.
    pub async fn new(our_callsign: &str, our_grid: Option<&str>) -> Self {
        Self::with_config(QsoManagerConfig {
            our_callsign: our_callsign.to_string(),
            our_grid: our_grid.map(|g| g.to_string()),
            ..Default::default()
        })
        .await
    }

    /// Build a harness from a full [`QsoManagerConfig`] (e.g. to tune the manual
    /// watchdog caps for the watchdog scenarios).
    pub async fn with_config(config: QsoManagerConfig) -> Self {
        let our_callsign = config.our_callsign.clone();
        let manager = QsoManager::new(config);
        manager
            .start()
            .await
            .expect("QsoManager::start should not fail in the sim harness");
        let events = manager.subscribe();
        Self {
            manager,
            events,
            exchange: MessageExchange::new(our_callsign.clone()),
            our_callsign,
            clock: SimClock::new(),
            pending_injects: Vec::new(),
            timeline: Timeline::default(),
            band: BandModel::clear(),
            allocator: SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default()),
        }
    }

    /// Access the underlying real [`QsoManager`] (for advanced assertions on
    /// QSO state that go beyond the [`Timeline`]).
    pub fn manager(&self) -> &QsoManager {
        &self.manager
    }

    /// The current slot index.
    pub fn slot(&self) -> u64 {
        self.clock.slot()
    }

    /// The current slot's parity.
    pub fn parity(&self) -> SlotParity {
        self.clock.parity()
    }

    /// The mutable virtual band model (occupy regions before `choose_tx_offset`).
    pub fn band_mut(&mut self) -> &mut BandModel {
        &mut self.band
    }

    /// A read-only view of the accumulated timeline so far.
    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// Consume the harness and take ownership of the timeline.
    pub fn into_timeline(self) -> Timeline {
        self.timeline
    }

    // --- Operator actions (thin wrappers over the real manual entry points) ---

    /// Operator initiates a **manual** call to a DX station that is calling CQ.
    /// The DX is assumed to be transmitting on the current slot's parity, so we
    /// latch the opposite. Returns the QSO id (string form).
    pub async fn call_station(&mut self, callsign: &str, freq_hz: f64) -> String {
        let dx_parity = self.clock.parity();
        let id = self
            .manager
            .respond_to_cq_manual(callsign.to_string(), freq_hz, Some(dx_parity))
            .await
            .expect("respond_to_cq_manual");
        id.to_string()
    }

    /// Operator initiates a **manual** CQ on the current slot's parity.
    pub async fn cq(&mut self, freq_hz: f64) -> String {
        let parity = self.clock.parity();
        let id = self
            .manager
            .start_cq_manual(freq_hz, Some(parity))
            .await
            .expect("start_cq_manual");
        id.to_string()
    }

    /// Operator replies to a station calling **us**, opening at `step`.
    pub async fn respond_to_caller(
        &mut self,
        callsign: &str,
        freq_hz: f64,
        step: ResponseStep,
        our_snr_of_them: Option<f32>,
        their_report: Option<i8>,
    ) -> String {
        let dx_parity = self.clock.parity();
        let id = self
            .manager
            .respond_to_caller(
                callsign.to_string(),
                freq_hz,
                Some(dx_parity),
                step,
                our_snr_of_them,
                their_report,
            )
            .await
            .expect("respond_to_caller");
        id.to_string()
    }

    /// Operator aborts (cancels) the active QSO with `callsign`, if one exists.
    pub async fn abort(&mut self, callsign: &str) {
        let target = self
            .manager
            .get_active_qsos()
            .await
            .into_iter()
            .find(|(_, p)| p.metadata.their_callsign.as_deref() == Some(callsign))
            .map(|(id, _)| id);
        if let Some(id) = target {
            self.manager.cancel_qso(id).await.expect("cancel_qso");
        }
    }

    // --- The virtual band ---

    /// Inject a decoded message into the **current** slot. It is delivered to
    /// the engine when [`Sim::tick`] runs. The text is parsed with the real FT8
    /// message parser, so `RR73` / `RRR` / `73` classify exactly as on-air.
    ///
    /// A decode whose text does not parse to a structured [`MessageType`] (e.g.
    /// free-text noise) is still recorded in the timeline's `receptions` and
    /// passed to the engine as a [`MessageType::NonStandard`], where the
    /// relevance filter ignores it — exactly the harmless outcome we want when
    /// modeling a crowded/noisy band.
    pub fn inject_decode(&mut self, text: &str, freq_hz: f64, snr_db: f32, dt: f32) {
        let parity = self.clock.parity();
        self.pending_injects.push(Reception {
            slot: self.clock.slot(),
            text: text.to_string(),
            freq_hz,
            snr_db,
            dt,
            slot_parity: parity,
        });
    }

    /// Inject many unrelated decodes across the passband in the current slot,
    /// and mark each region occupied in the [`BandModel`]. Models a crowded band.
    ///
    /// `entries` is a list of `(text, freq_hz)`; each is injected at a fixed
    /// moderate SNR and its offset is occupied in the band model.
    pub fn inject_crowd(&mut self, entries: &[(&str, f64)]) {
        for (text, freq) in entries {
            self.inject_decode(text, *freq, -12.0, 0.1);
            self.band.occupy(*freq, 30.0, 0.7);
        }
    }

    /// Inject two decodes at ~the same offset in the current slot (a collision).
    pub fn inject_collision(&mut self, text_a: &str, text_b: &str, freq_hz: f64) {
        self.inject_decode(text_a, freq_hz, -10.0, 0.1);
        self.inject_decode(text_b, freq_hz + 2.0, -14.0, 0.3);
    }

    // --- The allocator (TX-frequency selection / shift) ---

    /// Ask the real [`SmartFrequencyAllocator`] for the best TX offset given the
    /// current [`BandModel`], the offsets of our own active QSOs, and an
    /// optional DX target offset. Returns the chosen offset in Hz.
    ///
    /// This is the exact scoring the coordinator uses to pick (and shift) our TX
    /// frequency; the harness exposes it so crowded-band and shift scenarios can
    /// assert the choice.
    pub async fn choose_tx_offset(&self, dx_target_hz: Option<f64>) -> f64 {
        let own: Vec<f64> = self
            .manager
            .get_active_qsos()
            .await
            .into_iter()
            .map(|(_, p)| p.metadata.frequency)
            .collect();
        let ranked = self.allocator.rank_candidates(
            &self.band.spectral,
            &self.band.history,
            &own,
            dx_target_hz,
        );
        ranked
            .first()
            .map(|c| c.offset_hz)
            .expect("allocator always returns at least one candidate")
    }

    // --- Advancing time ---

    /// Run one slot: deliver this slot's injected decodes, run the engine's
    /// manual keep-call re-arm and the timeout watchdog at the slot's virtual
    /// `now`, drain all emitted events into the [`Timeline`], then advance the
    /// virtual clock to the next slot.
    pub async fn tick(&mut self) {
        let now = self.clock.now();
        let slot = self.clock.slot();

        // 1. Deliver this slot's decodes.
        let injects = std::mem::take(&mut self.pending_injects);
        for r in &injects {
            let msg = self
                .exchange
                .parse_message(&r.text)
                .unwrap_or(MessageType::NonStandard {
                    text: r.text.clone(),
                });
            self.manager
                .process_message(msg, r.text.clone(), r.freq_hz, Some(r.snr_db))
                .await
                .expect("process_message");
            self.timeline.receptions.push(r.clone());
        }

        // 2. Manual keep-call re-arm + timeout watchdog at the virtual `now`.
        self.manager.rearm_manual_calls_at(now).await;
        self.manager.check_timeouts_at(now).await;

        // 3. Drain all events emitted during this slot into the timeline.
        self.drain_events(slot);

        // 4. Advance the clock.
        self.timeline.last_slot = slot;
        self.clock.advance();
    }

    /// Run `n` consecutive empty slots (each calls [`Sim::tick`]). Handy for
    /// waiting out a watchdog with no further DX activity.
    pub async fn tick_n(&mut self, n: u64) {
        for _ in 0..n {
            self.tick().await;
        }
    }

    /// Drain pending events from the broadcast receiver, recording each into the
    /// timeline at `slot`.
    fn drain_events(&mut self, slot: u64) {
        loop {
            match self.events.try_recv() {
                Ok(event) => self.record_event(slot, event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => break,
                // Channel capacity is 1000; a lagged receiver only happens under
                // pathological volume. Keep draining the remaining buffered events.
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
    }

    fn record_event(&mut self, slot: u64, event: QsoEvent) {
        match event {
            QsoEvent::MessageToSend {
                qso_id: _,
                message,
                frequency,
                tx_parity,
            } => {
                let text = self
                    .exchange
                    .generate_message(&message)
                    .unwrap_or_else(|_| {
                        if let MessageType::NonStandard { text } = &message {
                            text.clone()
                        } else {
                            format!("{message:?}")
                        }
                    });
                self.timeline.transmissions.push(Transmission {
                    slot,
                    text,
                    message,
                    freq_hz: frequency,
                    tx_parity,
                });
            }
            QsoEvent::StateChanged {
                qso_id,
                old_state,
                new_state,
                ..
            } => {
                // A transition into `Failed` is how the manual watchdog, the
                // per-state timeouts, and operator cancellation all surface a
                // terminal failure — the engine emits a `StateChanged` into
                // `Failed`, not a separate `QsoFailed` event, for these paths.
                // Record it as a failure (carrying the reason from the state) so
                // `assert_failed_with` works uniformly across both surfaces.
                if let QsoState::Failed { reason, .. } = &new_state {
                    self.timeline.failures.push(Failure {
                        slot,
                        reason: reason.clone(),
                        their_callsign: old_state.their_callsign().map(|c| c.to_string()),
                        qso_id: qso_id.to_string(),
                    });
                }
                self.timeline.transitions.push(Transition {
                    slot,
                    qso_id: qso_id.to_string(),
                    old_state,
                    new_state,
                });
            }
            QsoEvent::QsoCompleted { qso_id, metadata } => {
                self.timeline.completions.push(Completion {
                    slot,
                    their_callsign: metadata.their_callsign.clone(),
                    qso_id: qso_id.to_string(),
                });
            }
            QsoEvent::QsoFailed {
                qso_id,
                reason,
                metadata,
            } => {
                self.timeline.failures.push(Failure {
                    slot,
                    reason,
                    their_callsign: metadata.their_callsign.clone(),
                    qso_id: qso_id.to_string(),
                });
            }
            QsoEvent::MessageReceived { .. } => {
                // Receptions are already recorded at inject time with full
                // band metadata (SNR/dt/parity); ignore the engine echo.
            }
            QsoEvent::DuplicateDetected { .. } => {
                // Surfaced via the absence of a new QSO id; not separately recorded.
            }
        }
    }

    // --- Scripted scenarios ---

    /// Run a declarative [`Scenario`] to completion and return the [`Timeline`].
    ///
    /// Each slot from 0 to the scenario's last referenced slot (plus a small
    /// settle tail) is ticked; actions scheduled for a slot fire during that
    /// slot, before the keep-call re-arm and watchdog run.
    pub async fn run_scenario(mut self, scenario: &Scenario) -> Timeline {
        let last = scenario.max_slot();
        // A short tail lets any final auto-reply / completion settle.
        let end = last + 2;
        for slot in 0..=end {
            // Apply actions scheduled for this slot.
            for (s, actions) in &scenario.steps {
                if *s == slot {
                    for action in actions.clone() {
                        self.apply_action(action).await;
                    }
                }
            }
            self.tick().await;
        }
        self.timeline
    }

    async fn apply_action(&mut self, action: SimAction) {
        match action {
            SimAction::CallStation { callsign, freq_hz } => {
                self.call_station(&callsign, freq_hz).await;
            }
            SimAction::Cq { freq_hz } => {
                self.cq(freq_hz).await;
            }
            SimAction::RespondToCaller {
                callsign,
                freq_hz,
                step,
                our_snr_of_them,
                their_report,
            } => {
                self.respond_to_caller(&callsign, freq_hz, step, our_snr_of_them, their_report)
                    .await;
            }
            SimAction::Abort { callsign } => {
                self.abort(&callsign).await;
            }
            SimAction::Inject {
                text,
                freq_hz,
                snr_db,
                dt,
            } => {
                self.inject_decode(&text, freq_hz, snr_db, dt);
            }
        }
    }

    /// Our station's callsign (convenience for building injected text).
    pub fn our_callsign(&self) -> &str {
        &self.our_callsign
    }
}

/// Assert a chosen TX offset stays at least `min_clear_hz` away from `occupied_hz`.
///
/// The allocator-side counterpart to [`Timeline`]'s assertions: use it after
/// [`Sim::choose_tx_offset`] to assert the allocator steered our TX clear of a
/// crowded/occupied region.
pub fn assert_tx_offset_clear_of(chosen_hz: f64, occupied_hz: f64, min_clear_hz: f64) {
    let gap = (chosen_hz - occupied_hz).abs();
    assert!(
        gap >= min_clear_hz,
        "expected chosen TX offset {chosen_hz:.0} Hz to be >= {min_clear_hz:.0} Hz \
         clear of occupied {occupied_hz:.0} Hz, but gap was only {gap:.0} Hz"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_clock_advances_in_slots_with_alternating_parity() {
        // The fixed epoch is an even-parity slot boundary, so with_base(epoch())
        // gives a fully deterministic clock.
        let mut clk = SimClock::with_base(SimClock::epoch());
        assert_eq!(clk.slot(), 0);
        assert_eq!(clk.now(), SimClock::epoch());
        assert_eq!(clk.parity(), SlotParity::Even);
        clk.advance();
        assert_eq!(clk.slot(), 1);
        assert_eq!(
            clk.now(),
            SimClock::epoch() + ChronoDuration::seconds(SLOT_SECONDS)
        );
        assert_eq!(clk.parity(), SlotParity::Odd);
        // Sanity: the sim clock's parity matches pancetta-core's slot parity.
        assert_eq!(SlotParity::of(clk.now()), SlotParity::Odd);
    }

    #[test]
    fn band_model_occupy_raises_power_and_activity() {
        let mut band = BandModel::clear();
        assert_eq!(band.spectral.peak_near(1500.0, 30.0), 0.0);
        band.occupy(1500.0, 30.0, 0.8);
        assert!(band.spectral.peak_near(1500.0, 30.0) > 0.5);
        assert!(band.history.activity_near(1500.0, 40.0) >= 1);
    }

    #[tokio::test]
    async fn empty_tick_produces_empty_timeline() {
        let mut sim = Sim::new("K5ARH", Some("EM10")).await;
        sim.tick().await;
        let tl = sim.into_timeline();
        assert!(tl.transmissions.is_empty());
        assert!(tl.completions.is_empty());
        assert!(tl.failures.is_empty());
    }

    #[test]
    fn assert_tx_offset_clear_of_passes_when_clear() {
        assert_tx_offset_clear_of(1800.0, 1500.0, 100.0);
    }

    #[test]
    #[should_panic(expected = "clear of occupied")]
    fn assert_tx_offset_clear_of_panics_when_too_close() {
        assert_tx_offset_clear_of(1520.0, 1500.0, 100.0);
    }
}
