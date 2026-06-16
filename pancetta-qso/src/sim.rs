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
//! # High-fidelity mode (`sim-hifi` feature)
//!
//! The default harness injects *decoded text* directly — it tests the QSO
//! engine, taking the decoder as given. With the `sim-hifi` feature,
//! `Sim::inject_signal` instead injects a transmitted **signal** (text + SNR +
//! [`FadingProfile`]) and runs it through the *real* pancetta-ft8 pipeline:
//! encode → modulate → apply fading → add calibrated AWGN → `decode_window`.
//! At low SNR or under deep fading the message may **MISS** entirely (no
//! decode), exactly as on a real band; the decoder's *actual* recovered
//! text/freq/SNR is what drives the engine. This tests the decoder's
//! weak-signal behavior together with the QSO logic. See `Sim::inject_signal`
//! for the SNR convention and determinism guarantees.
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

use crate::autonomous::{
    AutonomousOperator, DecodedMessageInfo, DxEvaluator, NullDxEvaluator, OperatorAction,
};
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

    /// Create a clock whose slot 0 is anchored to a wall-clock slot boundary
    /// **of Even parity**: floor `Utc::now()` to its slot boundary, then bump
    /// forward one slot if that boundary is Odd.
    ///
    /// This is the base the autonomous-drive harness uses. It keeps `base`
    /// within ~one slot of real `now` (so the QSO engine's real-clock
    /// `started_at` stamps land just after `base`, avoiding the negative-
    /// elapsed wrap the [`SimClock`] type docs warn about) while making the
    /// per-slot parity deterministic — slot 0 Even, slot 1 Odd, … — so the
    /// autonomous operator's Even-parity TX gating is reproducible.
    pub fn new_even() -> Self {
        let floored = Self::slot_floor(Utc::now());
        let base = if SlotParity::of(floored) == SlotParity::Even {
            floored
        } else {
            floored + ChronoDuration::seconds(SLOT_SECONDS)
        };
        Self { base, slot: 0 }
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
    /// Every high-fidelity signal injection ([`Sim::inject_signal`]), decoded
    /// or missed, in order. Empty for direct-inject (`inject_decode`) runs.
    pub signals: Vec<SignalOutcome>,
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

    /// Number of high-fidelity signal injections that the decoder recovered.
    pub fn signals_decoded(&self) -> usize {
        self.signals.iter().filter(|s| s.decoded).count()
    }

    /// Number of high-fidelity signal injections that were MISSED (lost in
    /// noise / fading — the decoder produced no copy of our message).
    pub fn signals_missed(&self) -> usize {
        self.signals.iter().filter(|s| !s.decoded).count()
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
            // High-fidelity signal outcomes for this slot render in the RX
            // column, showing decoded-vs-missed plus the requested SNR.
            let mut rx: Vec<String> = self
                .signals
                .iter()
                .filter(|s| s.slot == slot)
                .map(|s| {
                    if s.decoded {
                        format!(
                            "signal@{:+.0}dB DECODED -> {} ({:.0}Hz {:+.0}dB)",
                            s.requested_snr_db,
                            s.decoded_text.as_deref().unwrap_or(""),
                            s.measured_freq_hz.unwrap_or(s.sent_freq_hz),
                            s.measured_snr_db.unwrap_or(0.0),
                        )
                    } else {
                        format!(
                            "signal@{:+.0}dB MISSED [{}]",
                            s.requested_snr_db, s.sent_text
                        )
                    }
                })
                .collect();
            rx.extend(
                self.receptions
                    .iter()
                    .filter(|r| r.slot == slot)
                    // Don't double-print a reception that a hi-fi decode already
                    // surfaced (same slot+text).
                    .filter(|r| {
                        !self.signals.iter().any(|s| {
                            s.slot == slot
                                && s.decoded
                                && s.decoded_text.as_deref() == Some(r.text.as_str())
                        })
                    })
                    .map(|r| format!("{} ({:.0}Hz {:+.0}dB)", r.text, r.freq_hz, r.snr_db)),
            );
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

/// `true` if `text` is a CQ call (`"CQ ..."`, case-insensitive). Used by the
/// autonomous drive to route a CQ-self action to [`QsoManager::start_cq`].
fn is_cq_text(text: &str) -> bool {
    text.trim_start().to_uppercase().starts_with("CQ ")
}

/// Extract the DX callsign an autonomous TX is directed at: for a CQ-response
/// of the form `"<DX> <us> <grid>"` this is the first token; for a `"CQ ..."`
/// there is no directed DX. A "callsign-shaped" token contains a digit and a
/// letter and is at least 3 chars (filters `"CQ"`, `"DX"`, grids, reports).
fn first_callsign_of(text: &str) -> Option<String> {
    let mut tokens = text.split_whitespace();
    let first = tokens.next()?;
    if first.eq_ignore_ascii_case("CQ") {
        // "CQ <us> <grid>" or "CQ DX <us> <grid>": skip the CQ (+ optional
        // direction word) — these are our own CQs, not directed at a DX.
        return None;
    }
    if looks_like_callsign(first) {
        Some(first.to_uppercase())
    } else {
        None
    }
}

/// A token is callsign-shaped if it is ≥3 chars and contains both a digit and
/// a letter (so grids like `FN42`, reports like `-12`, and words like `DX`
/// don't qualify). Compound calls (`EA8/G8BCG`) qualify.
fn looks_like_callsign(tok: &str) -> bool {
    tok.len() >= 3
        && tok.chars().any(|c| c.is_ascii_digit())
        && tok.chars().any(|c| c.is_ascii_alphabetic())
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
    /// Base seed for the high-fidelity AWGN generator (deterministic replay).
    hifi_seed: u64,
    /// Optional autonomous-drive: when installed (via
    /// [`Sim::with_autonomous`]), [`Sim::tick`] also runs the *real*
    /// [`AutonomousOperator`] each slot — feeding it the slot's decodes at
    /// the virtual `now`, running [`AutonomousOperator::decide_at`], and
    /// executing the operator's [`OperatorAction::Transmit`] decisions
    /// against the same real [`QsoManager`]. `None` (default) leaves the
    /// harness in its original operator-driven mode, unchanged.
    autonomous: Option<AutonomousDrive>,
}

/// The autonomous-drive state bundled into a [`Sim`] when
/// [`Sim::with_autonomous`] is used: the real decision engine plus the DX
/// evaluator it scores CQs with.
struct AutonomousDrive {
    operator: AutonomousOperator,
    evaluator: Box<dyn DxEvaluator>,
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
            hifi_seed: 0xF18_C0DE,
            autonomous: None,
        }
    }

    /// Set the base seed for the high-fidelity AWGN generator
    /// ([`Sim::inject_signal`]). The same seed + same scenario replays
    /// byte-identically. Returns `self` for chaining after [`Sim::new`].
    pub fn with_hifi_seed(mut self, seed: u64) -> Self {
        self.hifi_seed = seed;
        self
    }

    /// Install the **real** [`AutonomousOperator`] as the decision-maker for
    /// this harness, with a [`NullDxEvaluator`] (every CQ scores 0.5).
    ///
    /// In autonomous-drive mode, each [`Sim::tick`] additionally:
    ///
    /// 1. feeds the slot's injected decodes to the operator at the virtual
    ///    `now` ([`AutonomousOperator::feed_decoded_messages_at`]),
    /// 2. tells it how many QSOs are currently active
    ///    ([`AutonomousOperator::set_active_qso_count`]),
    /// 3. runs one decision cycle at the slot's virtual time
    ///    ([`AutonomousOperator::decide_at`]), and
    /// 4. executes each emitted [`OperatorAction::Transmit`] against the
    ///    **same** real [`QsoManager`]: a hunt/pounce CQ-response (a
    ///    `"<DX> <us> <grid>"` message with `qso_id == None`) opens an
    ///    **autonomous** ([`crate::states::CallInitiation::Auto`]) QSO via
    ///    [`QsoManager::respond_to_cq`]; a `"CQ <us> <grid>"` opens an
    ///    autonomous CQ via [`QsoManager::start_cq`].
    ///
    /// Everything the manager then transmits ([`QsoEvent::MessageToSend`])
    /// lands in the [`Timeline`] exactly as in operator-driven mode. The
    /// operator-driven API ([`Sim::call_station`] etc.) is unchanged and may
    /// still be used; this only *adds* an autonomous decision step per tick.
    ///
    /// NOTE on Phase-5 gating: the QSO engine auto-sequences replies only for
    /// **manual** QSOs. An autonomous-opened QSO is `Auto`, so it emits its
    /// opening call but does **not** auto-advance through report → RR73 →
    /// completion. Scenarios that need an autonomous QSO to *complete* will
    /// therefore stall until Phase-5 flips that gate — see the
    /// `autonomous_scenarios` test suite.
    pub fn with_autonomous(self, operator: AutonomousOperator) -> Self {
        self.with_autonomous_evaluator(operator, Box::new(NullDxEvaluator))
    }

    /// Like [`Sim::with_autonomous`] but with a custom [`DxEvaluator`] (e.g.
    /// to score one DX higher than another for the pile-up "pick-one"
    /// scenario, or to drop a CQ below `min_dx_score`).
    pub fn with_autonomous_evaluator(
        mut self,
        operator: AutonomousOperator,
        evaluator: Box<dyn DxEvaluator>,
    ) -> Self {
        // Anchor the virtual clock to a wall-clock Even-parity slot boundary so
        // the operator's slot-parity TX gating (`SlotParity::from_unix_secs`)
        // is deterministic — slot 0 Even, slot 1 Odd, … — while keeping `base`
        // within ~one slot of real `now`. (The latter matters because the QSO
        // engine stamps `started_at`/`first_call_at` with the REAL clock; an
        // epoch-anchored base far in the past would make `virtual_now -
        // started_at` negative and wrap the timeout math — see the SimClock
        // type docs.) Without this, `Sim::new`'s base lands on a random parity,
        // making autonomous-drive runs flaky.
        self.clock = SimClock::new_even();
        self.autonomous = Some(AutonomousDrive {
            operator,
            evaluator,
        });
        self
    }

    /// `true` if this harness is in autonomous-drive mode.
    pub fn is_autonomous(&self) -> bool {
        self.autonomous.is_some()
    }

    /// Read-only access to the installed [`AutonomousOperator`], if any (for
    /// assertions on operator-internal state — e.g. `is_dx_busy`).
    pub fn operator(&self) -> Option<&AutonomousOperator> {
        self.autonomous.as_ref().map(|a| &a.operator)
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

        // 1b. Autonomous-drive: run the real decision engine over this slot's
        // decodes and execute its TX decisions against the same QsoManager.
        if self.autonomous.is_some() {
            self.run_autonomous_slot(&injects, now).await;
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

    /// Run one autonomous decision cycle for the current slot: feed the
    /// slot's decodes to the operator at the virtual `now`, ask it to decide,
    /// and execute each [`OperatorAction::Transmit`] against the real
    /// [`QsoManager`] using the production-equivalent entry points.
    ///
    /// Faithful to production wiring (`coordinator/autonomous.rs`): a
    /// CQ-response / hunt-pounce (`qso_id == None`, message of the form
    /// `"<DX> <us> ..."`) opens an [`crate::states::CallInitiation::Auto`]
    /// QSO via [`QsoManager::respond_to_cq`]; a `"CQ ..."` opens an autonomous
    /// CQ via [`QsoManager::start_cq`]. Both emit their opening
    /// [`QsoEvent::MessageToSend`], which the normal event drain records into
    /// the [`Timeline`]. The duplicate / DX-busy / recently-responded gates
    /// all live inside [`AutonomousOperator::decide_at`], so a suppressed CQ
    /// simply produces no `Transmit` here.
    async fn run_autonomous_slot(&mut self, injects: &[Reception], now: DateTime<Utc>) {
        // Build DecodedMessageInfo for the operator from this slot's decodes.
        // The `callsign` field is the *sender* of the decode (production sets
        // it from the decoder's `from_callsign`), parsed with the real
        // exchange parser — so a CQ's sender is the CQer and a reply's sender
        // is the replier. This is what `feed_decoded_messages` keys CQ
        // extraction and DX-busy tracking on.
        let infos: Vec<DecodedMessageInfo> = injects
            .iter()
            .map(|r| {
                let sender = self
                    .exchange
                    .parse_message(&r.text)
                    .ok()
                    .and_then(|m| m.sender_callsign().map(|s| s.to_string()));
                DecodedMessageInfo {
                    callsign: sender,
                    frequency_hz: r.freq_hz,
                    snr: r.snr_db.round() as i32,
                    message_text: r.text.clone(),
                    slot_parity: Some(r.slot_parity),
                    confidence: None,
                    time_offset_s: None,
                    decode_origin: None,
                }
            })
            .collect();

        // Keep the operator's active-QSO count in sync with the manager so its
        // max-concurrent and multi-slot thresholds gate correctly.
        let active = self.manager.get_active_qsos().await.len() as u32;

        // Decide. The drive bundle is taken out across the awaits below to
        // avoid holding a borrow on `self` while calling `&mut self` manager
        // entry points.
        let actions = {
            let drive = self
                .autonomous
                .as_mut()
                .expect("run_autonomous_slot only called when autonomous is Some");
            drive
                .operator
                .feed_decoded_messages_at(&infos, drive.evaluator.as_ref(), now);
            drive.operator.set_active_qso_count(active);
            drive.operator.decide_at(now.timestamp())
        };

        let dx_parity = self.clock.parity();
        for action in actions {
            if let OperatorAction::Transmit {
                message_text,
                frequency_offset,
                qso_id,
                ..
            } = action
            {
                // Only autonomous *initiations* (qso_id == None) are routed
                // through the manager here — exactly the items the coordinator
                // treats as initiations. (Mid-QSO sequencer items would carry
                // a qso_id, but the Auto path never produces those.)
                if qso_id.is_some() {
                    continue;
                }
                if is_cq_text(&message_text) {
                    // Calling CQ ourselves: open an autonomous CallingCq QSO.
                    let _ = self.manager.start_cq(frequency_offset, None).await;
                } else if let Some(dx) = first_callsign_of(&message_text) {
                    // Hunt/pounce: answer the DX's CQ as an autonomous QSO.
                    // The operator latched the DX's heard parity into the
                    // action's tx_parity (= dx_parity.opposite()); we pass the
                    // current slot parity as the DX parity, matching the
                    // operator-driven `call_station` convention.
                    let _ = self
                        .manager
                        .respond_to_cq(dx, frequency_offset, Some(dx_parity))
                        .await;
                }
            }
        }
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

    // --- High-fidelity virtual band (real encode -> noise/fading -> decode) ---

    /// Inject a transmitted **signal** (not pre-decoded text) into the current
    /// slot, running it through the *real* pancetta-ft8 pipeline:
    ///
    /// ```text
    ///   text --(Ft8Encoder)--> 79 symbols --(Ft8Modulator @ freq_hz)--> audio
    ///        --(apply FadingProfile)--> --(add calibrated AWGN @ snr_db)-->
    ///        --(Ft8Decoder::decode_window)--> decode(s) or NOTHING
    /// ```
    ///
    /// Unlike [`Sim::inject_decode`] (which hands the engine perfect text), this
    /// path exercises the decoder's weak-signal behavior: at low SNR or under
    /// deep fading the message may **MISS** entirely (no decode), exactly as on
    /// a real band. Whatever the decoder actually recovers — its text, its
    /// measured audio offset, and its *own* measured SNR — is what gets fed into
    /// the QSO engine, via the same [`process_message`](QsoManager::process_message)
    /// path [`inject_decode`](Sim::inject_decode) uses.
    ///
    /// # SNR convention
    ///
    /// `snr_db` is the **wideband RMS SNR** the research synthetic corpus uses
    /// (`gen_synth`): the AWGN RMS is set so that
    /// `20·log10(signal_rms / noise_rms) == snr_db` over the full 12 kHz buffer.
    /// This is a *generation* convention; the decoder reports its own
    /// spectrogram SNR (WSJT-X 2500 Hz reference,
    /// `estimate_snr_spectrogram`-style), which differs by a roughly constant
    /// offset (the signal occupies ~50 Hz while the noise spreads across the
    /// full Nyquist band, so the in-band SNR the decoder sees is higher). The
    /// engine is fed the decoder's measured SNR, not the requested one, so the
    /// QSO logic always sees a realistic value. The example prints both for
    /// comparison.
    ///
    /// # Decoder
    ///
    /// Decoding uses `Ft8Decoder::decode_window` — the native pancetta-ft8
    /// pipeline, which is the path the production coordinator runs (the ft8_lib
    /// FFI path is a research/baseline comparison tool, not the live decode).
    ///
    /// # Determinism
    ///
    /// The AWGN is generated from a per-call seed derived from
    /// `(self.hifi_seed, slot, freq, snr)`, so a given scenario replays
    /// byte-identically. Set the base seed with [`Sim::with_hifi_seed`].
    ///
    /// Returns the [`SignalOutcome`] for this injection (decoded vs missed),
    /// also recorded in the [`Timeline`].
    #[cfg(feature = "sim-hifi")]
    pub fn inject_signal(
        &mut self,
        text: &str,
        freq_hz: f64,
        snr_db: f32,
        fading: FadingProfile,
    ) -> SignalOutcome {
        let slot = self.clock.slot();
        let parity = self.clock.parity();

        // 1. Encode + modulate to a full 12.64 s / 12 kHz window at freq_hz.
        let mut encoder = pancetta_ft8::Ft8Encoder::new();
        let symbols = match encoder.encode_message(text, None) {
            Ok(s) => s,
            Err(e) => panic!("inject_signal: failed to encode {text:?}: {e}"),
        };
        // Place the signal at the absolute audio offset `freq_hz` by using it
        // as the modulator base (the lowest tone) and modulating at offset 0.
        // This matches `inject_decode`'s "audio offset in Hz" convention, and
        // the decoder reports `frequency_offset` as the absolute audio freq.
        // The 8-FSK tones span ~44 Hz above the base, and the modulator caps
        // the top tone at 2500 Hz, so usable `freq_hz` is ~[200, 2456].
        let mut modulator = pancetta_ft8::Ft8Modulator::new(
            pancetta_ft8::SAMPLE_RATE,
            freq_hz,
            pancetta_ft8::modulator::DEFAULT_TX_POWER,
        )
        .unwrap_or_else(|e| {
            panic!("inject_signal: FT8 modulator at {freq_hz} Hz should construct: {e}")
        });
        let mut audio = modulator
            .modulate_symbols(&symbols, 0.0)
            .unwrap_or_else(|e| panic!("inject_signal: failed to modulate {text:?}: {e}"));
        audio.resize(pancetta_ft8::WINDOW_SAMPLES, 0.0);

        // 2. Apply fading (time-varying attenuation / dropout) to the signal
        //    BEFORE adding noise, so the AWGN floor stays constant across the
        //    frame and the faded portions genuinely sink toward the noise.
        fading.apply(&mut audio, pancetta_ft8::SAMPLE_RATE);

        // 3. Add calibrated AWGN at the requested wideband SNR (research
        //    `gen_synth` convention). RMS is measured over the (post-fading)
        //    signal so a faded frame is noisier relative to its own energy.
        let seed = hifi_noise_seed(self.hifi_seed, slot, freq_hz, snr_db);
        add_calibrated_awgn(&mut audio, snr_db, seed);

        // 4. Decode through the REAL native pipeline (production path).
        let mut decoder = pancetta_ft8::Ft8Decoder::new(pancetta_ft8::Ft8Config::default())
            .expect("inject_signal: default FT8 decoder should construct");
        let decoded = decoder.decode_window(&audio).unwrap_or_default();

        // 5. Did our intended message survive? Match on the decoded TEXT
        //    (case-insensitive, trimmed) — a decode of *some other* phantom is
        //    not "our" signal getting through.
        let want = text.trim().to_uppercase();
        let hit = decoded
            .iter()
            .find(|d| d.text.trim().to_uppercase() == want);

        let outcome = match hit {
            Some(d) => {
                // Feed the decoder's ACTUAL recovered text/freq/SNR into the
                // engine, exactly like inject_decode would for a perfect copy.
                self.pending_injects.push(Reception {
                    slot,
                    text: d.text.clone(),
                    freq_hz: d.frequency_offset,
                    snr_db: d.snr_db,
                    dt: d.time_offset as f32,
                    slot_parity: parity,
                });
                SignalOutcome {
                    slot,
                    sent_text: text.to_string(),
                    sent_freq_hz: freq_hz,
                    requested_snr_db: snr_db,
                    fading,
                    decoded: true,
                    decoded_text: Some(d.text.clone()),
                    measured_snr_db: Some(d.snr_db),
                    measured_freq_hz: Some(d.frequency_offset),
                }
            }
            None => {
                // A real MISS: the slot simply has no reception of our signal.
                // The engine sees nothing this slot; keep-call re-arm / watchdog
                // still run on tick, so a QSO can recover over later slots.
                SignalOutcome {
                    slot,
                    sent_text: text.to_string(),
                    sent_freq_hz: freq_hz,
                    requested_snr_db: snr_db,
                    fading,
                    decoded: false,
                    decoded_text: None,
                    measured_snr_db: None,
                    measured_freq_hz: None,
                }
            }
        };
        self.timeline.signals.push(outcome.clone());
        outcome
    }
}

// ============================================================================
// High-fidelity sim: fading profiles, signal outcomes, calibrated noise.
//
// These plain-data / pure-math items are always compiled (no pancetta-ft8
// dependency); only [`Sim::inject_signal`], which drives the real codec, is
// gated behind the `sim-hifi` feature.
// ============================================================================

/// A time-varying channel impairment applied to a transmitted signal before
/// AWGN is added, in [`Sim::inject_signal`].
///
/// Fading is modeled as a per-sample real-valued amplitude envelope across the
/// 12.64 s FT8 frame. It is applied to the *signal* before the noise floor is
/// added, so faded portions genuinely sink toward (or below) the noise — the
/// realistic mechanism by which a weak-but-fading station drops in and out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FadingProfile {
    /// No fading: the signal passes through at full modeled amplitude.
    None,
    /// Flat (frequency-flat, time-flat) attenuation by `attenuation_db` over
    /// the whole frame. Positive values attenuate; the signal RMS drops, so at
    /// a fixed requested SNR this mostly just shifts where the calibration
    /// lands — useful for stacking a deterministic loss on top of an SNR sweep.
    Flat {
        /// Attenuation in dB (positive = quieter).
        attenuation_db: f32,
    },
    /// Dropout: the signal is fully present for the first `(1 - fraction)` of
    /// the frame, then absent (amplitude 0) for the trailing `fraction`.
    /// Models a station that fades out partway through the slot. With `fraction`
    /// large enough the decoder loses sync and the slot MISSES.
    Dropout {
        /// Fraction of the frame, at the end, during which the signal is gone
        /// (clamped to `[0.0, 1.0]`).
        fraction: f32,
    },
}

impl FadingProfile {
    /// Apply this profile in-place to `samples`, a `sample_rate`-Hz audio
    /// buffer spanning one FT8 frame. (`sample_rate` is threaded in so this
    /// stays independent of the optional pancetta-ft8 dependency.)
    pub fn apply(&self, samples: &mut [f32], _sample_rate: u32) {
        match *self {
            FadingProfile::None => {}
            FadingProfile::Flat { attenuation_db } => {
                let gain = 10f32.powf(-attenuation_db / 20.0);
                for s in samples.iter_mut() {
                    *s *= gain;
                }
            }
            FadingProfile::Dropout { fraction } => {
                let frac = fraction.clamp(0.0, 1.0);
                let n = samples.len();
                let keep = ((1.0 - frac) * n as f32) as usize;
                for s in samples.iter_mut().skip(keep) {
                    *s = 0.0;
                }
            }
        }
    }
}

/// The outcome of one high-fidelity signal injection ([`Sim::inject_signal`]):
/// what we transmitted, and whether the real decoder recovered it.
#[derive(Debug, Clone)]
pub struct SignalOutcome {
    /// Slot index in which the signal was injected.
    pub slot: u64,
    /// The text we encoded and transmitted.
    pub sent_text: String,
    /// The audio offset we modulated at, in Hz.
    pub sent_freq_hz: f64,
    /// The requested (wideband-RMS) SNR, in dB.
    pub requested_snr_db: f32,
    /// The fading profile applied.
    pub fading: FadingProfile,
    /// `true` if the decoder recovered our message; `false` if it was MISSED.
    pub decoded: bool,
    /// The decoder's recovered text (when `decoded`).
    pub decoded_text: Option<String>,
    /// The decoder's *measured* SNR (WSJT-X 2500 Hz reference), when `decoded`.
    /// This — not `requested_snr_db` — is what the QSO engine is fed.
    pub measured_snr_db: Option<f32>,
    /// The decoder's measured audio offset, when `decoded`.
    pub measured_freq_hz: Option<f64>,
}

/// Add additive white Gaussian noise to `samples` at the requested wideband
/// RMS SNR, matching the research `gen_synth` convention: the noise RMS is set
/// so `20·log10(signal_rms / noise_rms) == snr_db` over the whole buffer.
///
/// Uses a deterministic seeded Box–Muller generator (no external RNG crate) so
/// a given seed replays byte-identically.
#[cfg(feature = "sim-hifi")]
fn add_calibrated_awgn(samples: &mut [f32], snr_db: f32, seed: u64) {
    if samples.is_empty() {
        return;
    }
    let signal_rms =
        (samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / samples.len() as f64).sqrt();
    if signal_rms <= 0.0 {
        return;
    }
    let noise_rms = signal_rms / 10f64.powf(snr_db as f64 / 20.0);
    let mut rng = DeterministicGaussian::new(seed);
    for s in samples.iter_mut() {
        *s += (rng.next_standard() * noise_rms) as f32;
    }
}

/// Derive a per-injection noise seed from the base seed and the call's
/// distinguishing parameters, so distinct signals in distinct slots get
/// distinct (but reproducible) noise realizations.
#[cfg(feature = "sim-hifi")]
fn hifi_noise_seed(base: u64, slot: u64, freq_hz: f64, snr_db: f32) -> u64 {
    let mut h = base ^ 0x9E37_79B9_7F4A_7C15;
    h = h.wrapping_mul(0x100_0000_01B3).wrapping_add(slot);
    h = h
        .wrapping_mul(0x100_0000_01B3)
        .wrapping_add((freq_hz as u64).wrapping_mul(1_000));
    h = h
        .wrapping_mul(0x100_0000_01B3)
        .wrapping_add(((snr_db * 100.0) as i64) as u64);
    h
}

/// A tiny deterministic standard-normal generator: SplitMix64 uniform draws fed
/// through Box–Muller. Self-contained so the harness needs no `rand` dependency
/// and replays identically given a seed.
#[cfg(feature = "sim-hifi")]
struct DeterministicGaussian {
    state: u64,
    spare: Option<f64>,
}

#[cfg(feature = "sim-hifi")]
impl DeterministicGaussian {
    fn new(seed: u64) -> Self {
        Self {
            // Avoid the all-zero state degeneracy of SplitMix64.
            state: seed ^ 0xDEAD_BEEF_CAFE_F00D,
            spare: None,
        }
    }

    /// Next uniform in (0, 1), via SplitMix64.
    fn next_uniform(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // 53-bit mantissa -> (0,1); nudge off exact 0 so log is finite.
        let u = (z >> 11) as f64 / (1u64 << 53) as f64;
        u.max(f64::MIN_POSITIVE)
    }

    /// Next draw from the standard normal N(0,1).
    fn next_standard(&mut self) -> f64 {
        if let Some(v) = self.spare.take() {
            return v;
        }
        let u1 = self.next_uniform();
        let u2 = self.next_uniform();
        let mag = (-2.0 * u1.ln()).sqrt();
        let z0 = mag * (2.0 * std::f64::consts::PI * u2).cos();
        let z1 = mag * (2.0 * std::f64::consts::PI * u2).sin();
        self.spare = Some(z1);
        z0
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

/// High-fidelity sim tests: real encode -> noise/fading -> decode -> engine.
///
/// Marginal-SNR decode is inherently probabilistic, so the weak-signal tests
/// assert *handling* (no panic, miss recorded, keep-call continues) and ranges,
/// not exact bit outcomes. All noise is seeded, so each test replays
/// identically; the strong-signal completion test is the deterministic anchor.
#[cfg(all(test, feature = "sim-hifi"))]
mod hifi_tests {
    use super::*;

    /// Flat attenuation scales every sample by the expected linear gain.
    #[test]
    fn fading_flat_attenuates_by_db() {
        let mut buf = vec![1.0f32; 100];
        FadingProfile::Flat {
            attenuation_db: 6.0,
        }
        .apply(&mut buf, pancetta_ft8::SAMPLE_RATE);
        // -6 dB ~= 0.501x amplitude.
        assert!((buf[0] - 0.5012).abs() < 0.01, "got {}", buf[0]);
        assert!(buf.iter().all(|&s| (s - buf[0]).abs() < 1e-6));
    }

    /// Dropout zeroes the trailing `fraction` of the buffer and leaves the rest.
    #[test]
    fn fading_dropout_zeroes_tail() {
        let mut buf = vec![1.0f32; 1000];
        FadingProfile::Dropout { fraction: 0.3 }.apply(&mut buf, pancetta_ft8::SAMPLE_RATE);
        assert_eq!(buf[0], 1.0);
        assert_eq!(buf[699], 1.0); // last kept sample (700 kept)
        assert_eq!(buf[700], 0.0); // first dropped sample
        assert_eq!(buf[999], 0.0);
    }

    /// Calibrated AWGN hits the requested wideband RMS SNR (within tolerance)
    /// and is deterministic for a fixed seed.
    #[test]
    fn awgn_calibration_and_determinism() {
        // A flat-amplitude "signal" so RMS is well defined.
        let make = || vec![0.1f32; 12_000];
        let mut a = make();
        add_calibrated_awgn(&mut a, -10.0, 42);
        let mut b = make();
        add_calibrated_awgn(&mut b, -10.0, 42);
        // Byte-identical replay for the same seed.
        assert_eq!(a, b, "same seed must produce identical noise");

        // Measured SNR ~= requested. signal_rms = 0.1; recover noise RMS from
        // the residual after subtracting the constant signal.
        let noise: Vec<f64> = a.iter().map(|&s| s as f64 - 0.1).collect();
        let noise_rms = (noise.iter().map(|n| n * n).sum::<f64>() / noise.len() as f64).sqrt();
        let measured_snr = 20.0 * (0.1 / noise_rms).log10();
        assert!(
            (measured_snr - (-10.0)).abs() < 0.5,
            "measured wideband SNR {measured_snr:.2} dB should be ~ -10 dB"
        );

        // A different seed gives a different realization.
        let mut c = make();
        add_calibrated_awgn(&mut c, -10.0, 43);
        assert_ne!(a, c, "different seeds should differ");
    }

    /// A strong, clean signal reliably decodes and drives a manual-call QSO to
    /// completion. This is the deterministic anchor (seeded noise, +6 dB).
    #[tokio::test]
    async fn strong_signal_completes_qso() {
        let mut sim = Sim::new("K5ARH", Some("EM10")).await.with_hifi_seed(11);
        let dx = "VB7F";
        let freq = 1200.0;
        sim.call_station(dx, freq).await;

        let g = sim.inject_signal(&format!("K5ARH {dx} EM73"), freq, 6.0, FadingProfile::None);
        assert!(g.decoded, "strong grid reply must decode");
        sim.tick().await;

        let r = sim.inject_signal(&format!("K5ARH {dx} -12"), freq, 6.0, FadingProfile::None);
        assert!(r.decoded, "strong report must decode");
        sim.tick().await;

        let rr = sim.inject_signal(&format!("K5ARH {dx} RR73"), freq, 6.0, FadingProfile::None);
        assert!(rr.decoded, "strong RR73 must decode");
        sim.tick().await;
        sim.tick_n(2).await;

        let tl = sim.into_timeline();
        assert_eq!(
            tl.signals_missed(),
            0,
            "strong signals should never miss\n{tl}"
        );
        tl.assert_completed_with(dx);
        tl.assert_no_duplicate_qsos();
    }

    /// The decoder reports its own (spectrogram, 2500 Hz reference) SNR, which
    /// is higher than the requested wideband SNR — and that measured value is
    /// what reaches the engine.
    #[tokio::test]
    async fn measured_snr_drives_engine_not_requested() {
        let mut sim = Sim::new("K5ARH", Some("EM10")).await.with_hifi_seed(5);
        let dx = "VB7F";
        let freq = 1500.0;
        sim.call_station(dx, freq).await;
        let out = sim.inject_signal(
            &format!("K5ARH {dx} EM73"),
            freq,
            -10.0,
            FadingProfile::None,
        );
        assert!(out.decoded);
        let measured = out.measured_snr_db.unwrap();
        // In-band SNR exceeds the wideband requested SNR by the BW correction.
        assert!(
            measured > out.requested_snr_db,
            "measured {measured} should exceed requested {}",
            out.requested_snr_db
        );
        // The reception fed to the engine carries the measured SNR (receptions
        // are committed to the timeline on tick).
        sim.tick().await;
        let tl = sim.timeline();
        assert!(tl
            .receptions
            .iter()
            .any(|r| (r.snr_db - measured).abs() < 1e-3));
    }

    /// A deep dropout fade causes a MISS, and the harness handles it without
    /// panicking: the slot has no reception, the miss is recorded, and the
    /// manual keep-call keeps re-arming (so the QSO is not stranded).
    #[tokio::test]
    async fn deep_dropout_misses_and_keep_call_continues() {
        let mut sim = Sim::new("K5ARH", Some("EM10")).await.with_hifi_seed(2);
        let dx = "VB7F";
        let freq = 1000.0;
        sim.call_station(dx, freq).await;

        // Heavy dropout (90% of the frame gone) — the DX's answer is lost.
        let out = sim.inject_signal(
            &format!("K5ARH {dx} EM73"),
            freq,
            6.0,
            FadingProfile::Dropout { fraction: 0.9 },
        );
        assert!(!out.decoded, "a 90% dropout must MISS");
        sim.tick().await;
        // A couple more silent slots: keep-call must keep firing.
        sim.tick_n(2).await;

        let tl = sim.into_timeline();
        assert_eq!(tl.signals_missed(), 1);
        assert_eq!(tl.signals_decoded(), 0);
        // The miss left no reception this run.
        assert!(
            tl.receptions.is_empty(),
            "a missed signal yields no reception\n{tl}"
        );
        // Manual keep-call re-armed our CQ-response across the silent slots.
        assert!(
            tl.count_transmitted_containing(dx) >= 2,
            "expected keep-call to re-send to {dx} across slots\n{tl}"
        );
        // And nothing completed (the DX never got through).
        tl.assert_not_completed_with(dx);
    }

    /// Same seed + same scenario => identical decoded/missed outcomes.
    #[tokio::test]
    async fn marginal_run_is_reproducible() {
        async fn run(seed: u64) -> Vec<bool> {
            let mut sim = Sim::new("K5ARH", Some("EM10")).await.with_hifi_seed(seed);
            let dx = "VB7F";
            let freq = 1500.0;
            sim.call_station(dx, freq).await;
            let mut outcomes = Vec::new();
            for snr in [-20.0f32, -24.0, -22.0, -23.0] {
                let o =
                    sim.inject_signal(&format!("K5ARH {dx} EM73"), freq, snr, FadingProfile::None);
                outcomes.push(o.decoded);
                sim.tick().await;
            }
            outcomes
        }
        let a = run(99).await;
        let b = run(99).await;
        assert_eq!(
            a, b,
            "same seed must replay identical decoded/missed pattern"
        );
    }
}
