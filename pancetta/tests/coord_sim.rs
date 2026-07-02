//! Coordinator-level QSO simulation harness — mock-rig PTT, stale-TX,
//! multi-stream, TX-policy.
//!
//! # What this is
//!
//! A durable, reusable fixture ([`CoordSim`]) that stands up the *coordinator
//! layer* of pancetta's transmit path against the **mock rig** (no rigctld, no
//! physical radio) and lets a scenario:
//!
//! - Start / advance QSOs through the **real** [`QsoManager`] (the same entry
//!   points the coordinator uses: `respond_to_cq_with`, `respond_to_caller`,
//!   `start_cq`, `cancel_qso`, `process_message`).
//! - Pump the **real** coordinator active-set populater logic
//!   ([`CoordSim::pump_qso_events`]), which mirrors
//!   `coordinator/qso.rs`: insert a `qso_id` into `active_tx_qsos` on a
//!   `StateChanged` into an active state (and on `QsoCompleted`); remove it on
//!   `StateChanged → Failed` / `QsoFailed`; forward each `MessageToSend` to the
//!   TX worker as a `TransmitRequest`.
//! - **Drive transmit slots deterministically** and **assert at the rig level**:
//!   did PTT key (`mock.get_ptt() == On`) for each scheduled QSO transmit? at
//!   what audio offset? was PTT released after?
//!
//! It complements the engine-level harness in `pancetta-qso/src/sim.rs` (which
//! exercises the QSO *state machine* over a virtual band). This one exercises
//! the *coordinator's* TX scheduler gate + mock-rig PTT + multi-stream path —
//! the layer between the state machine and the radio.
//!
//! # How determinism / keying is handled (READ THIS)
//!
//! The production TX worker (`coordinator/tx.rs`) sleeps in real wall-clock time
//! until the next UTC FT8 slot boundary (`schedule_tx` + `interruptible_sleep`),
//! then keys PTT, ships audio, sleeps the ~12.6s burst, and unkeys. A test that
//! waited on real slot alignment would be slow and flaky.
//!
//! So — exactly as `tests/tx_ptt_integration.rs` does — this harness **replicates
//! the worker's keying *decision chain* faithfully but compresses the timing**:
//!
//!   1. **Step 0 — TX-policy hard mute.** If the shared `tx_policy` atomic reads
//!      `Disabled`, the request is consumed, a failed `TransmitComplete` is the
//!      logical outcome, and **no PTT is keyed**. (Same gate the worker applies
//!      to `TransmitRequest` / `MultiTransmitRequest` / `TuneRequest`.)
//!   2. **Coalesce.** A backlog of `TransmitRequest`s is collapsed with the
//!      **real production** [`coalesce_transmit_requests`] (re-exported from the
//!      coordinator for this harness): newest-per-`qso_id` wins, terminal-QSO
//!      requests are dropped, `qso_id == None` (manual) sends are preserved.
//!   3. **Step 4b — drop-stale-TX gate.** Each surviving item is checked with the
//!      **real production** [`tx_qso_is_live`] against the **real** shared
//!      `active_tx_qsos` set. An item whose `qso_id` is no longer live is dropped
//!      *before keying* — no PTT.
//!   4. **Step 5/7/9 — key / audio / unkey.** For each surviving item we send
//!      `RigControl(SetPtt{true})` over the **real** [`MessageBus`] to the
//!      **real** [`MockRig`] consumer (mirror of `coordinator/hamlib.rs`), record
//!      the keyed offset, sleep a *small bounded* interval (tens of ms, NOT a
//!      slot) so the consumer observes the ON edge, then `SetPtt{false}`.
//!
//! No `schedule_tx` UTC math, no slot sleep — *which slot* a transmit lands in
//! is `schedule_tx`'s job and is unit-tested exhaustively in `tx.rs`'s
//! `schedule_tx_tests`. This harness asserts the orthogonal contract: *given the
//! decision to transmit, does PTT key at the right offset for live QSOs and stay
//! silent for dead ones / disabled policy*. Time is therefore bounded by a few
//! `await`s of a few ms each; pass/fail never depends on wall-clock slot phase.

#![allow(clippy::expect_used)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use pancetta_hamlib::{MockRig, PttState, RigControl, Vfo};
use pancetta_lib::coordinator::{
    active_tx_qso_key, coalesce_transmit_requests, remote_tx_permitted, tx_qso_is_live,
    CoalesceEntry,
};
use pancetta_lib::message_bus::{
    ComponentId, ComponentMessage, MessageBus, MessageType, RigControlMessage,
};
use pancetta_qso::{CallInitiation, QsoEvent, QsoManager, QsoManagerConfig, QsoMetadata, QsoState};

use pancetta_core::slot::SlotParity;
use pancetta_core::TxPolicy;

/// How long the harness lets the mock-rig consumer observe a PTT edge. Bounded,
/// not a slot — keeps the whole suite sub-second.
const PTT_OBSERVE_MS: u64 = 30;

// ---------------------------------------------------------------------------
// Timeline (mirrors the style of pancetta-qso::sim::Timeline)
// ---------------------------------------------------------------------------

/// One PTT-keyed transmit the harness drove, as the mock rig observed it.
#[derive(Debug, Clone)]
pub struct KeyedTx {
    /// Slot index (monotonic counter the scenario advances).
    pub slot: u64,
    /// FT8 message text that was keyed.
    pub text: String,
    /// Absolute audio offset (Hz) the request carried — the value the worker
    /// would feed to `modulator.set_base_frequency`.
    pub freq_hz: f64,
    /// QSO id this transmit belonged to (`None` = manual / tune / free-text).
    pub qso_id: Option<String>,
    /// Did the mock rig actually report PTT `On` during this transmit?
    pub ptt_keyed: bool,
    /// Was PTT observed back `Off` after the transmit completed?
    pub ptt_released: bool,
}

/// One request that was DROPPED before keying (policy mute or stale-TX gate),
/// recorded so a scenario can assert "this did NOT key" with a readable reason.
#[derive(Debug, Clone)]
pub struct DroppedTx {
    pub slot: u64,
    pub text: String,
    pub qso_id: Option<String>,
    /// Why it was dropped.
    pub reason: DropReason,
}

/// Why a request never keyed PTT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Global TX policy was `Disabled` (hard mute).
    PolicyDisabled,
    /// The request's QSO was not in `active_tx_qsos` (superseded / cancelled /
    /// completed-past-grace), so the Step 4b gate dropped it.
    StaleQso,
    /// The request was superseded by a newer one for the same QSO during
    /// backlog coalescing (older keep-call frame).
    CoalescedAway,
    /// A `TxOrigin::Remote` request that the station-agent arm gate did not
    /// permit (unarmed / no local consent / expired / etc.) — fail-closed drop.
    RemoteNotArmed,
}

/// Accumulated, readable record of everything a scenario drove. Mirrors the
/// engine-level `sim::Timeline` so the two harnesses feel consistent.
#[derive(Debug, Default)]
pub struct Timeline {
    /// Every transmit that reached the keying chain and keyed PTT, in order.
    pub keyed: Vec<KeyedTx>,
    /// Every request dropped before keying (policy / stale / coalesced).
    pub dropped: Vec<DroppedTx>,
    /// Highest slot index reached.
    pub last_slot: u64,
}

impl Timeline {
    /// Every keyed transmit belonging to `qso_id`.
    pub fn keyed_for_qso(&self, qso_id: &str) -> Vec<&KeyedTx> {
        let key = active_tx_qso_key(qso_id);
        self.keyed
            .iter()
            .filter(|k| k.qso_id.as_deref().map(active_tx_qso_key) == Some(key.clone()))
            .collect()
    }

    /// Did any transmit for `qso_id` key PTT?
    pub fn keyed_any_for_qso(&self, qso_id: &str) -> bool {
        self.keyed_for_qso(qso_id).iter().any(|k| k.ptt_keyed)
    }

    /// Did we key PTT for ANY transmit at all?
    pub fn keyed_anything(&self) -> bool {
        self.keyed.iter().any(|k| k.ptt_keyed)
    }

    /// The distinct audio offsets we keyed in `slot`.
    pub fn offsets_in_slot(&self, slot: u64) -> Vec<f64> {
        self.keyed
            .iter()
            .filter(|k| k.slot == slot && k.ptt_keyed)
            .map(|k| k.freq_hz)
            .collect()
    }

    // --- Assertion helpers (panic with a readable timeline on failure) ---

    /// Assert PTT keyed at least once for `qso_id`.
    pub fn assert_keyed_for_qso(&self, qso_id: &str) {
        assert!(
            self.keyed_any_for_qso(qso_id),
            "expected PTT to key for QSO {qso_id}, but it never did.\n{self}"
        );
    }

    /// Assert PTT NEVER keyed for `qso_id`.
    pub fn assert_not_keyed_for_qso(&self, qso_id: &str) {
        assert!(
            !self.keyed_any_for_qso(qso_id),
            "expected PTT to NEVER key for QSO {qso_id}, but it did.\n{self}"
        );
    }

    /// Assert NOTHING keyed PTT across the whole run (TX-disabled scenario).
    pub fn assert_silent(&self) {
        assert!(
            !self.keyed_anything(),
            "expected the rig to stay silent, but PTT keyed.\n{self}"
        );
    }

    /// Assert a transmit for `qso_id` keyed at exactly `expected` Hz (within
    /// 0.001 Hz — the offset is carried verbatim, no scheduling shift).
    pub fn assert_keyed_at_offset(&self, qso_id: &str, expected: f64) {
        let keyed = self.keyed_for_qso(qso_id);
        let found = keyed
            .iter()
            .any(|k| k.ptt_keyed && (k.freq_hz - expected).abs() < 0.001);
        assert!(
            found,
            "expected QSO {qso_id} to key at {expected} Hz, but offsets were {:?}.\n{self}",
            keyed.iter().map(|k| k.freq_hz).collect::<Vec<_>>()
        );
    }

    /// Assert PTT was released (observed back Off) after every keyed transmit.
    pub fn assert_all_released(&self) {
        for k in &self.keyed {
            if k.ptt_keyed {
                assert!(
                    k.ptt_released,
                    "PTT keyed for '{}' (qso {:?}) but was never released.\n{self}",
                    k.text, k.qso_id
                );
            }
        }
    }

    /// Assert exactly `n` distinct offsets keyed in `slot` (multi-stream count).
    pub fn assert_distinct_offsets_in_slot(&self, slot: u64, n: usize) {
        let mut offs = self.offsets_in_slot(slot);
        offs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        offs.dedup();
        assert_eq!(
            offs.len(),
            n,
            "expected {n} distinct keyed offset(s) in slot {slot}, saw {:?}.\n{self}",
            offs
        );
    }
}

impl std::fmt::Display for Timeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "=== Coord TX Timeline (slots 0..={}) ===",
            self.last_slot
        )?;
        writeln!(
            f,
            "{:>4} | {:<6} | {:<22} | {:>7} | {:<10} | qso",
            "slot", "PTT", "text", "freq", "released?"
        )?;
        for k in &self.keyed {
            writeln!(
                f,
                "{:>4} | {:<6} | {:<22} | {:>7.0} | {:<10} | {}",
                k.slot,
                if k.ptt_keyed { "ON" } else { "(none)" },
                truncate(&k.text, 22),
                k.freq_hz,
                if k.ptt_released { "released" } else { "STUCK" },
                k.qso_id.as_deref().unwrap_or("-"),
            )?;
        }
        for d in &self.dropped {
            writeln!(
                f,
                "{:>4} | DROP   | {:<22} | {:>7} | {:<10} | {} ({:?})",
                d.slot,
                truncate(&d.text, 22),
                "-",
                "-",
                d.qso_id.as_deref().unwrap_or("-"),
                d.reason,
            )?;
        }
        Ok(())
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n.saturating_sub(1)])
    }
}

// ---------------------------------------------------------------------------
// CoordSim fixture
// ---------------------------------------------------------------------------

/// Coordinator-level QSO simulation fixture.
///
/// Owns: a real [`MessageBus`], a real [`QsoManager`] + its event receiver, a
/// real [`MockRig`] behind a hamlib consumer, the shared `active_tx_qsos` set,
/// and the shared `tx_policy` atomic — i.e. everything the coordinator's TX
/// gate consults. See the module docs for the determinism model.
pub struct CoordSim {
    /// The real message bus all components share.
    pub bus: MessageBus,
    /// The real QSO state machine.
    pub manager: QsoManager,
    /// Our event receiver (drained by [`CoordSim::pump_qso_events`]).
    qso_rx: tokio::sync::broadcast::Receiver<QsoEvent>,
    /// The mock rig the hamlib consumer drives.
    rig: Arc<MockRig>,
    /// Shared TX-active set (drop-stale-TX gate), exactly as the coordinator
    /// holds it.
    pub active_tx_qsos: Arc<RwLock<HashSet<String>>>,
    /// Shared tri-state TX policy atomic, exactly as the coordinator holds it.
    pub tx_policy: Arc<AtomicU8>,
    /// Shutdown flag for the spawned hamlib consumer.
    shutdown: Arc<AtomicBool>,
    /// Our callsign (for building expected message text).
    pub our_callsign: String,
    /// Accumulated timeline.
    pub timeline: Timeline,
    /// Monotonic slot counter the scenario advances.
    slot: u64,
    /// RX dial frequency shared into QsoManager (0 = unknown).
    pub dial_frequency_hz: Arc<AtomicU64>,
    /// Split TX dial frequency shared into QsoManager (0 = simplex).
    pub split_tx_frequency_hz: Arc<AtomicU64>,
    /// Completed-QSO metadata collected by `pump_qso_events` from
    /// `QsoEvent::QsoCompleted`. Keyed by `qso_id.to_string()`.
    pub completed: Vec<QsoMetadata>,
    /// Station-agent remote-TX arm gate, exactly as the coordinator holds it.
    /// `drive_slot` consults it (via the real `remote_tx_permitted`) for any
    /// `TxOrigin::Remote` pending item before keying PTT. Fresh = unarmed = deny.
    pub remote_tx_arm: Arc<Mutex<pancetta_agent::arm::ArmState>>,
}

impl CoordSim {
    /// Build a fixture with the given station callsign. Spawns the mock-rig
    /// hamlib consumer.
    pub async fn new(our_callsign: &str) -> Self {
        let bus = MessageBus::new(512).expect("bus");
        let config = QsoManagerConfig {
            our_callsign: our_callsign.to_string(),
            our_grid: Some("EM10".to_string()),
            ..Default::default()
        };
        let mut manager = QsoManager::new(config);
        let qso_rx = manager.subscribe();

        // Wire shared dial-frequency atomics so completed-QSO RF stamps work.
        let dial_frequency_hz = Arc::new(AtomicU64::new(0));
        let split_tx_frequency_hz = Arc::new(AtomicU64::new(0));
        manager.set_dial_frequency_source(Arc::clone(&dial_frequency_hz));
        manager.set_split_tx_frequency_source(Arc::clone(&split_tx_frequency_hz));

        let shutdown = Arc::new(AtomicBool::new(false));
        let rig = spawn_hamlib_consumer(&bus, shutdown.clone()).await;

        Self {
            bus,
            manager,
            qso_rx,
            rig,
            active_tx_qsos: Arc::new(RwLock::new(HashSet::new())),
            tx_policy: Arc::new(AtomicU8::new(TxPolicy::Full.as_u8())),
            shutdown,
            our_callsign: our_callsign.to_string(),
            timeline: Timeline::default(),
            slot: 0,
            dial_frequency_hz,
            split_tx_frequency_hz,
            completed: Vec::new(),
            // Fresh (unarmed) arm — remote TX is denied until a scenario arms it.
            remote_tx_arm: Arc::new(Mutex::new(pancetta_agent::arm::ArmState::new())),
        }
    }

    /// Set the RX dial frequency (Hz) — mirrors the hamlib poll loop updating
    /// the coordinator's `operating_frequency_hz` atomic.
    pub fn set_rx_dial(&self, hz: u64) {
        self.dial_frequency_hz.store(hz, Ordering::Relaxed);
    }

    /// Set the split TX dial frequency (Hz). Zero = simplex (RX dial is used).
    /// Mirrors the coordinator setting `split_tx_frequency_hz` on band change /
    /// split toggle.
    pub fn set_split_tx(&self, hz: u64) {
        self.split_tx_frequency_hz.store(hz, Ordering::Relaxed);
    }

    /// Set the global TX policy (mirrors the operator's `g` cycle / Shift+Q).
    pub fn set_policy(&self, policy: TxPolicy) {
        self.tx_policy.store(policy.as_u8(), Ordering::Release);
    }

    /// Fully arm the remote-TX gate for a scenario: verified TX-scope grant +
    /// local consent + a fresh heartbeat, so `remote_tx_permitted` returns true.
    /// (Simulates a future P3 arm; in production nothing constructs the grant.)
    pub fn arm_remote_tx(&self) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut st = self.remote_tx_arm.lock().expect("arm lock");
        st.arm(
            pancetta_agent::arm::VerifiedArmGrant {
                operator_callsign: "K5ARH".to_string(),
                ttl_ms: 120_000,
                scope_tx: true,
                jti: "sim-arm-jti".to_string(),
            },
            now_ms,
        );
        st.set_local_consent(true, now_ms);
        st.heartbeat("sim-arm-jti", 1, now_ms);
    }

    /// Drain all currently-buffered `QsoEvent`s and apply the coordinator's
    /// active-set populater logic, returning the ordered list of
    /// `TransmitRequest`s the coordinator would have forwarded to the TX worker.
    ///
    /// This is a faithful mirror of `coordinator/qso.rs`'s event loop:
    /// - `StateChanged → active`: insert qso_id into `active_tx_qsos`
    /// - `StateChanged → Failed`: remove qso_id
    /// - `QsoCompleted`: insert qso_id (grace window — we keep it live; the
    ///   scenario controls removal)
    /// - `QsoFailed`: remove qso_id
    /// - `MessageToSend`: emit a `TransmitRequest` carrying the latched
    ///   `tx_parity`
    ///
    /// Events are processed in order so the active-set insert is ordered ahead
    /// of the `TransmitRequest` it enables — the exact ordering the
    /// StateChanged-at-QSO-start fix guarantees.
    pub fn pump_qso_events(&mut self) -> Vec<PendingTx> {
        let mut pending = Vec::new();
        loop {
            match self.qso_rx.try_recv() {
                Ok(ev) => self.apply_event(ev, &mut pending),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            }
        }
        pending
    }

    fn apply_event(&mut self, ev: QsoEvent, pending: &mut Vec<PendingTx>) {
        match ev {
            QsoEvent::StateChanged {
                qso_id, new_state, ..
            } => {
                let key = active_tx_qso_key(&qso_id.to_string());
                if new_state.is_active() {
                    self.active_tx_qsos.write().unwrap().insert(key);
                } else if matches!(new_state, QsoState::Failed { .. }) {
                    self.active_tx_qsos.write().unwrap().remove(&key);
                }
            }
            QsoEvent::QsoCompleted { qso_id, metadata } => {
                // Grace window: keep the key live so the final 73 still keys.
                // The scenario removes it explicitly via `expire_qso` when it
                // wants to model grace elapsing.
                let key = active_tx_qso_key(&qso_id.to_string());
                self.active_tx_qsos.write().unwrap().insert(key);
                // Capture the completed metadata so scenarios can assert on the
                // stamped RF frequency (dial + audio offset).
                self.completed.push(metadata);
            }
            QsoEvent::QsoFailed { qso_id, .. } => {
                let key = active_tx_qso_key(&qso_id.to_string());
                self.active_tx_qsos.write().unwrap().remove(&key);
            }
            QsoEvent::MessageToSend {
                qso_id,
                message,
                frequency,
                tx_parity,
                remote_origin,
            } => {
                // The coordinator renders the QsoMessage to FT8 text via
                // pancetta_qso::utils::generate_ft8_message. We carry the raw
                // message debug for the timeline; the keying decision doesn't
                // depend on the rendered text, only on qso_id / freq / policy /
                // active-set, so a stable label is sufficient and avoids
                // re-deriving the (private-ish) text rules here.
                //
                // SECURITY (B1 mirror): derive the TransmitRequest origin from
                // the QSO's `remote_origin` exactly as coordinator/qso.rs does,
                // so a remote QSO's forwarded TX is `Remote` (arm-gated) and a
                // local QSO's is `Local` (byte-identical).
                let text = render_message(&message, &self.our_callsign);
                pending.push(PendingTx {
                    text,
                    frequency_offset: frequency,
                    qso_id: Some(qso_id.to_string()),
                    tx_parity,
                    origin: if remote_origin {
                        pancetta_lib::message_bus::TxOrigin::Remote
                    } else {
                        pancetta_lib::message_bus::TxOrigin::Local
                    },
                });
            }
            _ => {}
        }
    }

    /// Remove a QSO from the active-TX set, modelling its post-completion grace
    /// window elapsing (the coordinator's spawned 16s delayed purge) or an
    /// explicit cancel that didn't surface as a `Failed` StateChanged.
    pub fn expire_qso(&mut self, qso_id: &str) {
        let key = active_tx_qso_key(qso_id);
        self.active_tx_qsos.write().unwrap().remove(&key);
    }

    /// Inject a manual (`qso_id == None`) free-text / tune-like transmit intent
    /// directly into the next slot's pending list. Mirrors the operator typing
    /// a free-text message or pressing tune.
    pub fn manual_tx(&self, text: &str, freq_hz: f64) -> PendingTx {
        PendingTx {
            text: text.to_string(),
            frequency_offset: freq_hz,
            qso_id: None,
            tx_parity: None,
            origin: pancetta_lib::message_bus::TxOrigin::Local,
        }
    }

    /// Build a **remote-originated** manual TX request (for arm-gate scenarios).
    /// `qso_id == None` so only the arm gate (not the drop-stale gate) governs.
    pub fn remote_tx(&self, text: &str, freq_hz: f64) -> PendingTx {
        PendingTx {
            text: text.to_string(),
            frequency_offset: freq_hz,
            qso_id: None,
            tx_parity: None,
            origin: pancetta_lib::message_bus::TxOrigin::Remote,
        }
    }

    /// Drive ONE transmit slot deterministically with the given pending
    /// requests (as if they had all been enqueued to the TX worker's channel
    /// before the worker woke). Applies the full keying-decision chain (policy
    /// mute → coalesce → Step-4b gate → key/audio/unkey) and records the
    /// outcome on the timeline. Returns the slot index driven.
    ///
    /// `pending` is ordered oldest-first, exactly like the FIFO crossbeam
    /// channel the worker drains.
    pub async fn drive_slot(&mut self, pending: Vec<PendingTx>) -> u64 {
        let slot = self.slot;
        self.slot += 1;
        self.timeline.last_slot = slot;

        // --- Step 0: TX-policy hard mute (Disabled) ---
        // Policy primacy: Disabled drops EVERYTHING first, including Remote items
        // with a live arm.
        if current_policy(&self.tx_policy) == TxPolicy::Disabled {
            for p in &pending {
                self.timeline.dropped.push(DroppedTx {
                    slot,
                    text: p.text.clone(),
                    qso_id: p.qso_id.clone(),
                    reason: DropReason::PolicyDisabled,
                });
            }
            return slot;
        }

        // --- Step 0a: Remote-TX arm gate ---
        // Drop any `TxOrigin::Remote` pending item the station-agent arm gate
        // does not permit (fail-closed), exactly as the TX worker does before
        // keying PTT. `Local` items pass through untouched (byte-identical).
        let now_ms = chrono::Utc::now().timestamp_millis();
        let pending: Vec<PendingTx> = pending
            .into_iter()
            .filter(|p| {
                if p.origin == pancetta_lib::message_bus::TxOrigin::Remote
                    && !remote_tx_permitted(&self.remote_tx_arm, now_ms)
                {
                    self.timeline.dropped.push(DroppedTx {
                        slot,
                        text: p.text.clone(),
                        qso_id: p.qso_id.clone(),
                        reason: DropReason::RemoteNotArmed,
                    });
                    false
                } else {
                    true
                }
            })
            .collect();

        // --- Coalesce backlog with the REAL production coalescer ---
        // Preserve each entry's origin so a folded bundle carries it (fail-safe).
        let drained: Vec<CoalesceEntry> = pending
            .iter()
            .map(|p| CoalesceEntry {
                message_text: p.text.clone(),
                frequency_offset: p.frequency_offset,
                qso_id: p.qso_id.clone(),
                tx_parity: p.tx_parity,
                origin: p.origin,
            })
            .collect();
        let active = self.active_tx_qsos.clone();
        let outcome = coalesce_transmit_requests(drained, |id| tx_qso_is_live_shared(id, &active));

        // Record what coalescing / the gate removed, with a best-effort reason.
        // (We can't tell apart "coalesced" vs "dropped_terminal" per-entry from
        // the outcome counters, so we reconstruct per-entry below by replaying
        // the same predicate + a newest-per-qso map.)
        self.record_removed(slot, &pending, &outcome.retained);

        // --- Step 4b + Step 5/7/9 for each surviving (live) item ---
        for entry in &outcome.retained {
            let live = tx_qso_is_live_shared(entry.qso_id.as_deref(), &self.active_tx_qsos);
            if !live {
                // Defensive: coalescer already drops terminal ids, but the
                // worker re-checks at key time. Mirror that.
                self.timeline.dropped.push(DroppedTx {
                    slot,
                    text: entry.message_text.clone(),
                    qso_id: entry.qso_id.clone(),
                    reason: DropReason::StaleQso,
                });
                continue;
            }
            let (keyed, released) = self
                .key_once(&entry.message_text, entry.frequency_offset)
                .await;
            self.timeline.keyed.push(KeyedTx {
                slot,
                text: entry.message_text.clone(),
                freq_hz: entry.frequency_offset,
                qso_id: entry.qso_id.clone(),
                ptt_keyed: keyed,
                ptt_released: released,
            });
        }

        slot
    }

    /// Reconstruct which pending entries were removed (coalesced vs stale) and
    /// log them on the timeline, so a scenario can assert "the older frame did
    /// NOT key".
    fn record_removed(&mut self, slot: u64, pending: &[PendingTx], retained: &[CoalesceEntry]) {
        use std::collections::HashMap;
        // Map each pending entry to its fate.
        let active = self.active_tx_qsos.clone();
        // Last index per qso key (newest wins) — anything earlier was coalesced.
        let mut last_idx: HashMap<String, usize> = HashMap::new();
        for (i, p) in pending.iter().enumerate() {
            if let Some(id) = &p.qso_id {
                last_idx.insert(active_tx_qso_key(id), i);
            }
        }
        let retained_texts: HashSet<String> =
            retained.iter().map(|e| e.message_text.clone()).collect();
        for (i, p) in pending.iter().enumerate() {
            if retained_texts.contains(&p.text) {
                continue; // survived
            }
            let reason = match &p.qso_id {
                None => continue, // manual is never removed by coalescer
                Some(id) => {
                    if !tx_qso_is_live_shared(Some(id.as_str()), &active) {
                        DropReason::StaleQso
                    } else if last_idx.get(&active_tx_qso_key(id)) != Some(&i) {
                        DropReason::CoalescedAway
                    } else {
                        continue;
                    }
                }
            };
            self.timeline.dropped.push(DroppedTx {
                slot,
                text: p.text.clone(),
                qso_id: p.qso_id.clone(),
                reason,
            });
        }
    }

    /// Send PTT-on → (audio) → PTT-off over the real bus to the real mock rig,
    /// observing the ON edge and the OFF release. Returns `(keyed, released)`.
    async fn key_once(&self, text: &str, freq_hz: f64) -> (bool, bool) {
        // Step 5: assert PTT.
        send_ptt(&self.bus, true).await;
        // Race a poller for the ON edge concurrently with a small settle.
        let keyed = wait_for_ptt(&self.rig, PttState::On, Duration::from_secs(2)).await;

        // Step 7: route audio (best-effort; no audio consumer attached).
        let _ = self
            .bus
            .send_message(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Audio,
                MessageType::AudioOutput {
                    samples: vec![0.0f32; 16],
                    sample_rate: 12_000,
                },
                Instant::now(),
            ))
            .await;
        // Bounded settle so the ON edge is unambiguously observed.
        tokio::time::sleep(Duration::from_millis(PTT_OBSERVE_MS)).await;

        // Step 9: de-assert PTT.
        send_ptt(&self.bus, false).await;
        let released = wait_for_ptt(&self.rig, PttState::Off, Duration::from_secs(2)).await;
        let _ = (text, freq_hz); // recorded by caller
        (keyed, released)
    }

    /// Open an **autonomous** (Auto) QSO exactly as the coordinator's
    /// `QsoMessage::StartAutonomousQso` handler does for a pounce: a plain
    /// `respond_to_cq` (= `CallInitiation::Auto`) at the DX's decoded frequency.
    /// Returns the new QSO id.
    pub async fn autonomous_pounce(
        &self,
        dx: &str,
        dx_freq_hz: f64,
        dx_parity: Option<SlotParity>,
    ) -> String {
        self.manager
            .respond_to_cq(dx.to_string(), dx_freq_hz, dx_parity)
            .await
            .expect("autonomous respond_to_cq")
            .to_string()
    }

    /// Feed a decoded DX frame into the QSO engine exactly as the coordinator's
    /// decode loop does (`parse_ft8_message` → `process_message`). The resulting
    /// auto-sequenced reply (if any) surfaces on the next `pump_qso_events`.
    pub async fn inject_decode(&self, text: &str, freq_hz: f64) {
        let msg = pancetta_qso::utils::parse_ft8_message(text, &self.our_callsign)
            .unwrap_or_else(|e| panic!("parse_ft8_message('{text}'): {e}"));
        self.manager
            .process_message(msg, text.to_string(), freq_hz, Some(-12.0))
            .await
            .expect("process_message");
    }

    /// Tear down the consumer and take ownership of the accumulated timeline.
    /// Optional — scenarios may also read `sim.timeline` directly; the
    /// fixture's `Drop` flips the shutdown flag regardless.
    pub fn finish(mut self) -> Timeline {
        self.shutdown.store(true, Ordering::Release);
        std::mem::take(&mut self.timeline)
    }
}

impl Drop for CoordSim {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

/// A coordinator-forwarded transmit request, pre-keying.
#[derive(Debug, Clone)]
pub struct PendingTx {
    pub text: String,
    pub frequency_offset: f64,
    pub qso_id: Option<String>,
    pub tx_parity: Option<SlotParity>,
    /// Origin of this request. `Local` (default) skips the remote-TX arm gate;
    /// `Remote` is gated by the coordinator's `ArmState` in `drive_slot`.
    pub origin: pancetta_lib::message_bus::TxOrigin,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn current_policy(p: &Arc<AtomicU8>) -> TxPolicy {
    TxPolicy::from_u8(p.load(Ordering::Acquire))
}

/// Shared-set wrapper around the REAL `tx_qso_is_live` (fails open on a poisoned
/// lock, exactly like the worker's helper).
fn tx_qso_is_live_shared(qso_id: Option<&str>, active: &Arc<RwLock<HashSet<String>>>) -> bool {
    match active.read() {
        Ok(set) => tx_qso_is_live(qso_id, &set),
        Err(_) => true,
    }
}

/// Render a `QsoMessage` to a stable label for the timeline. The keying decision
/// never depends on this text, so a debug label is sufficient (and avoids
/// re-deriving the FT8 text-format rules, which live behind
/// `pancetta_qso::utils::generate_ft8_message`).
fn render_message(message: &pancetta_qso::MessageType, _our: &str) -> String {
    format!("{message:?}")
        .split_whitespace()
        .next()
        .unwrap_or("MSG")
        .to_string()
}

/// Send a `RigControl(SetPtt)` over the bus, exactly as the TX worker does.
async fn send_ptt(bus: &MessageBus, state: bool) {
    let msg = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Hamlib,
        MessageType::RigControl(RigControlMessage::SetPtt { state }),
        Instant::now(),
    );
    bus.send_message(msg).await.expect("send PTT");
}

/// Poll the mock rig until it reports `want` or the timeout elapses.
async fn wait_for_ptt(rig: &MockRig, want: PttState, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(state) = rig.get_ptt(Vfo::Current).await {
            if state == want {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    false
}

/// Spawn a consumer on the `Hamlib` channel mirroring `coordinator/hamlib.rs`'s
/// `SetPtt` handling, backed by a shared [`MockRig`].
async fn spawn_hamlib_consumer(bus: &MessageBus, shutdown: Arc<AtomicBool>) -> Arc<MockRig> {
    let (_tx, rx) = bus
        .create_channel(ComponentId::Hamlib)
        .await
        .expect("create Hamlib channel");

    let rig = Arc::new(MockRig::default());
    rig.connect().await.expect("mock rig connect");

    let rig_for_task = Arc::clone(&rig);
    tokio::spawn(async move {
        while !shutdown.load(Ordering::Acquire) {
            match rx.try_recv() {
                Ok(message) => {
                    if let MessageType::RigControl(RigControlMessage::SetPtt { state }) =
                        message.message_type
                    {
                        let ptt = if state { PttState::On } else { PttState::Off };
                        let _ = rig_for_task.set_ptt(Vfo::Current, ptt).await;
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }
    });

    rig
}

// ===========================================================================
// Permanent scenarios
// ===========================================================================

/// PTT keys for a scheduled QSO transmit (the StateChanged-at-QSO-start fix):
/// `respond_to_cq_with` emits StateChanged(active) BEFORE the first
/// MessageToSend, so `pump_qso_events` inserts the qso_id into `active_tx_qsos`
/// before the TransmitRequest reaches the Step-4b gate, and PTT keys.
#[tokio::test]
async fn ptt_keys_for_scheduled_qso() {
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "W1AW".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
        )
        .await
        .expect("respond_to_cq_with");

    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "QSO start should have forwarded at least one TransmitRequest"
    );

    sim.drive_slot(pending).await;

    sim.timeline.assert_keyed_for_qso(&qso_id.to_string());
    sim.timeline.assert_all_released();
    // And it keyed at the requested offset.
    sim.timeline
        .assert_keyed_at_offset(&qso_id.to_string(), 1500.0);
}

/// Stale-TX drop: a superseded / cancelled / completed-past-grace QSO's queued
/// TransmitRequest is dropped before keying — PTT does NOT key for it.
#[tokio::test]
async fn stale_tx_dropped_after_supersede_no_ptt() {
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "DL1ABC".to_string(),
            1200.0,
            Some(SlotParity::Odd),
            CallInitiation::Auto,
            None,
            false,
        )
        .await
        .expect("respond_to_cq_with");

    // Coordinator forwards the first call; capture it but DON'T transmit yet.
    let pending = sim.pump_qso_events();
    assert!(!pending.is_empty());

    // Now the QSO is superseded / cancelled: its key leaves the active set
    // (as a StateChanged→Failed would, or the grace purge). Model that.
    sim.expire_qso(&qso_id.to_string());

    // Drive the slot with the now-stale request. It must be dropped, not keyed.
    sim.drive_slot(pending).await;

    sim.timeline.assert_not_keyed_for_qso(&qso_id.to_string());
    sim.timeline.assert_silent();
    // And the drop was recorded as a stale-QSO drop.
    assert!(
        sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::StaleQso),
        "expected a StaleQso drop.\n{}",
        sim.timeline
    );
}

/// TX backpressure / coalescing: a backlog of keep-call frames for the SAME
/// live QSO coalesces to the newest; the older stale frames are NOT transmitted.
#[tokio::test]
async fn coalesce_backlog_newest_wins_stale_not_keyed() {
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "JA1XYZ".to_string(),
            1800.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
        )
        .await
        .expect("respond_to_cq_with");
    let id = qso_id.to_string();
    // Make the QSO live.
    let _ = sim.pump_qso_events();

    // Simulate three keep-call frames backed up for the same QSO (oldest first).
    let backlog = vec![
        PendingTx {
            text: "KEEPCALL-OLD-1".to_string(),
            frequency_offset: 1800.0,
            qso_id: Some(id.clone()),
            tx_parity: Some(SlotParity::Even),
            origin: pancetta_lib::message_bus::TxOrigin::Local,
        },
        PendingTx {
            text: "KEEPCALL-OLD-2".to_string(),
            frequency_offset: 1800.0,
            qso_id: Some(id.clone()),
            tx_parity: Some(SlotParity::Even),
            origin: pancetta_lib::message_bus::TxOrigin::Local,
        },
        PendingTx {
            text: "KEEPCALL-NEWEST".to_string(),
            frequency_offset: 1800.0,
            qso_id: Some(id.clone()),
            tx_parity: Some(SlotParity::Even),
            origin: pancetta_lib::message_bus::TxOrigin::Local,
        },
    ];

    sim.drive_slot(backlog).await;

    // Exactly one keyed transmit for this QSO this slot — the newest.
    let keyed = sim.timeline.keyed_for_qso(&id);
    assert_eq!(
        keyed.len(),
        1,
        "expected exactly one keyed frame after coalesce, got {}.\n{}",
        keyed.len(),
        sim.timeline
    );
    assert_eq!(keyed[0].text, "KEEPCALL-NEWEST");
    assert!(keyed[0].ptt_keyed);

    // The two older frames were coalesced away (not keyed).
    let coalesced = sim
        .timeline
        .dropped
        .iter()
        .filter(|d| d.reason == DropReason::CoalescedAway)
        .count();
    assert_eq!(
        coalesced, 2,
        "expected 2 coalesced-away frames.\n{}",
        sim.timeline
    );
}

/// Multi-stream: two concurrent live QSOs on different offsets each key + key
/// PTT on their own frequency in a single slot.
#[tokio::test]
async fn two_simultaneous_qsos_key_on_distinct_freqs() {
    let mut sim = CoordSim::new("K5ARH").await;
    let a = sim
        .manager
        .respond_to_cq_with(
            "VK3ABC".to_string(),
            1000.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
        )
        .await
        .expect("qso a");
    let b = sim
        .manager
        .respond_to_cq_with(
            "ZL2DEF".to_string(),
            2400.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
        )
        .await
        .expect("qso b");

    // Pump events: both go active, both forward a first TransmitRequest.
    let pending = sim.pump_qso_events();
    assert!(
        pending.len() >= 2,
        "expected ≥2 forwarded requests, got {}",
        pending.len()
    );

    let slot = sim.drive_slot(pending).await;

    sim.timeline.assert_keyed_for_qso(&a.to_string());
    sim.timeline.assert_keyed_for_qso(&b.to_string());
    sim.timeline.assert_keyed_at_offset(&a.to_string(), 1000.0);
    sim.timeline.assert_keyed_at_offset(&b.to_string(), 2400.0);
    // Two distinct offsets keyed in the one slot.
    sim.timeline.assert_distinct_offsets_in_slot(slot, 2);
    sim.timeline.assert_all_released();
}

/// TX policy — Disabled: no PTT keys at all, for any source.
#[tokio::test]
async fn tx_policy_disabled_is_silent() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.set_policy(TxPolicy::Disabled);

    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "G0XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
        )
        .await
        .expect("respond_to_cq_with");
    let _ = qso_id;

    let mut pending = sim.pump_qso_events();
    // Also throw in a manual free-text send — Disabled mutes EVERYTHING.
    pending.push(sim.manual_tx("CQ K5ARH EM10", 1234.0));

    sim.drive_slot(pending).await;

    sim.timeline.assert_silent();
    assert!(
        sim.timeline
            .dropped
            .iter()
            .all(|d| d.reason == DropReason::PolicyDisabled),
        "every drop under Disabled should be PolicyDisabled.\n{}",
        sim.timeline
    );
}

/// TX policy — RespondOnly: an in-progress QSO's transmit still keys (it's not
/// an initiation), but a NEW initiation (manual CQ / hunt) does not. Initiation
/// gating is enforced at the *sources* in production (tui_relay / autonomous),
/// so this scenario models that contract: we only forward in-progress QSO TX
/// (the `qso_id == Some` MessageToSend path) and suppress initiations upstream.
#[tokio::test]
async fn tx_policy_respond_only_keeps_qso_drops_initiation() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.set_policy(TxPolicy::RespondOnly);

    // In-progress QSO (answering a station) — this is a RESPONSE, allowed.
    let qso_id = sim
        .manager
        .respond_to_caller(
            "F5ABC".to_string(),
            1600.0,
            Some(SlotParity::Odd),
            pancetta_core::ResponseStep::Grid,
            None,
            None,
            None,
            false,
        )
        .await
        .expect("respond_to_caller");

    let mut pending = sim.pump_qso_events();

    // A NEW initiation under RespondOnly: production suppresses it at the source
    // (TxPolicy::allows_initiation() == false). Model the gate explicitly: only
    // append the initiation if the policy permits it.
    let policy = current_policy(&sim.tx_policy);
    let initiation = sim.manual_tx("CQ K5ARH EM10", 1234.0);
    if policy.allows_initiation() {
        pending.push(initiation.clone());
    }
    // Record the suppression in a way the assertion can see: it must NOT key.

    sim.drive_slot(pending).await;

    // The in-progress QSO keyed.
    sim.timeline.assert_keyed_for_qso(&qso_id.to_string());
    sim.timeline
        .assert_keyed_at_offset(&qso_id.to_string(), 1600.0);
    // The CQ initiation never keyed (it was suppressed at the source).
    assert!(
        !sim.timeline.keyed.iter().any(|k| k.text.contains("CQ")),
        "a CQ initiation must NOT key under RespondOnly.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
}

/// TX policy — Full: both in-progress QSO TX and a new initiation key.
#[tokio::test]
async fn tx_policy_full_keys_everything() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.set_policy(TxPolicy::Full);

    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "EA4AAA".to_string(),
            1700.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
        )
        .await
        .expect("respond_to_cq_with");

    let mut pending = sim.pump_qso_events();
    // Manual CQ initiation is allowed under Full.
    let policy = current_policy(&sim.tx_policy);
    if policy.allows_initiation() {
        pending.push(sim.manual_tx("CQ K5ARH EM10", 1234.0));
    }

    sim.drive_slot(pending).await;

    sim.timeline.assert_keyed_for_qso(&qso_id.to_string());
    // The manual CQ (qso_id == None) also keyed.
    assert!(
        sim.timeline
            .keyed
            .iter()
            .any(|k| k.text.contains("CQ") && k.ptt_keyed),
        "manual CQ must key under Full.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
}

/// Frequency fidelity: the offset requested for a QSO is the offset actually
/// keyed — no silent shift. Covers the SmartFrequencyAllocator-chosen offset
/// being honored end-to-end at the coordinator layer.
#[tokio::test]
async fn requested_offset_is_used() {
    let mut sim = CoordSim::new("K5ARH").await;
    // An "allocator-chosen" odd offset that must survive verbatim.
    let chosen_offset = 2317.0;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "PY2QWE".to_string(),
            chosen_offset,
            Some(SlotParity::Odd),
            CallInitiation::Auto,
            None,
            false,
        )
        .await
        .expect("respond_to_cq_with");

    let pending = sim.pump_qso_events();
    // The forwarded request must carry the exact chosen offset.
    assert!(
        pending
            .iter()
            .any(|p| (p.frequency_offset - chosen_offset).abs() < 0.001),
        "coordinator must forward the exact requested offset, got {:?}",
        pending
            .iter()
            .map(|p| p.frequency_offset)
            .collect::<Vec<_>>()
    );

    sim.drive_slot(pending).await;

    sim.timeline
        .assert_keyed_at_offset(&qso_id.to_string(), chosen_offset);
}

/// Manual / tune (qso_id == None) is never gated by the active-set: it keys even
/// with an empty active set (mirrors "manual PTT and tune DO key").
#[tokio::test]
async fn manual_send_never_gated_keys_with_empty_active_set() {
    let mut sim = CoordSim::new("K5ARH").await;
    // No QSOs at all → active set empty.
    let manual = sim.manual_tx("CQ K5ARH EM10", 1500.0);
    sim.drive_slot(vec![manual]).await;

    assert!(
        sim.timeline.keyed.iter().any(|k| k.ptt_keyed),
        "manual send must key even with an empty active set.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
}

// ===========================================================================
// Phase 5 — autonomous QSO completion through the coordinator TX path.
// These exercise the SAME path the production wiring uses: the
// StartAutonomousQso handler opens an Auto QSO (respond_to_cq), the universal
// decode loop (process_message) auto-sequences it, and each reply keys PTT at
// the rig. They are the coordinator-level counterpart to the engine-level
// pancetta-qso/tests/autonomous_scenarios.rs.
// ===========================================================================

/// An autonomous pounce runs the full ladder — grid → R-report → 73 — and PTT
/// keys at the rig for each, all on the DX's frequency (Tx=Rx). This is the
/// end-to-end Phase-5 acceptance at the coordinator level.
#[tokio::test]
async fn autonomous_pounce_completes_end_to_end_with_ptt() {
    let mut sim = CoordSim::new("K5ARH").await;

    // Slot 0: the StartAutonomousQso handler opens the Auto QSO; opening keys.
    let qso_id = sim
        .autonomous_pounce("VB7F", 1500.0, Some(SlotParity::Odd))
        .await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;
    sim.timeline.assert_keyed_for_qso(&qso_id);

    // Slot 1: DX sends us a report → engine auto-sends our R-report; it keys.
    sim.inject_decode("K5ARH VB7F -12", 1500.0).await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    // Slot 2: DX rogers with RR73 → engine completes + auto-sends our 73; keys.
    sim.inject_decode("K5ARH VB7F RR73", 1500.0).await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    // The full responder ladder was keyed, in order, all at the DX's freq
    // (Tx=Rx), all released. (The harness records the MessageType variant as the
    // keyed `text`: CqResponse = our grid, ReportAck = our R-report,
    // SeventyThree = our 73 close.)
    let keyed: Vec<_> = sim.timeline.keyed_for_qso(&qso_id);
    let ladder: Vec<&str> = keyed.iter().map(|k| k.text.as_str()).collect();
    assert_eq!(
        ladder,
        vec!["CqResponse", "ReportAck", "SeventyThree"],
        "expected the full grid → R-report → 73 ladder keyed in order.\n{}",
        sim.timeline
    );
    assert!(
        keyed
            .iter()
            .all(|k| k.ptt_keyed && (k.freq_hz - 1500.0).abs() < 0.5),
        "all autonomous-QSO transmits must key PTT on the DX's 1500 Hz freq.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
}

/// No double-send: the opening goes out exactly ONCE. (In production the
/// autonomous task sends StartAutonomousQso INSTEAD OF a raw TransmitRequest;
/// here we confirm the QsoManager-emitted opening is the only keyed TX in the
/// opening slot.)
#[tokio::test]
async fn autonomous_pounce_opening_keys_exactly_once() {
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .autonomous_pounce("VB7F", 1500.0, Some(SlotParity::Odd))
        .await;

    let pending = sim.pump_qso_events();
    let slot = sim.drive_slot(pending).await;

    let keyed_this_slot: Vec<_> = sim
        .timeline
        .keyed
        .iter()
        .filter(|k| k.slot == slot && k.ptt_keyed)
        .collect();
    assert_eq!(
        keyed_this_slot.len(),
        1,
        "the autonomous opening must key exactly once (no double-send).\n{}",
        sim.timeline
    );
    assert_eq!(keyed_this_slot[0].qso_id.as_deref(), Some(qso_id.as_str()));
}

/// Under TX policy Disabled, an autonomous QSO's transmit is hard-muted at the
/// TX worker — PTT never keys, even though the QSO exists. (Initiation is also
/// suppressed earlier by the planner; this asserts the worker-level backstop.)
#[tokio::test]
async fn autonomous_qso_tx_hard_muted_when_policy_disabled() {
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .autonomous_pounce("VB7F", 1500.0, Some(SlotParity::Odd))
        .await;
    sim.set_policy(TxPolicy::Disabled);

    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    sim.timeline.assert_not_keyed_for_qso(&qso_id);
    assert!(
        sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::PolicyDisabled),
        "expected a PolicyDisabled drop for the autonomous QSO.\n{}",
        sim.timeline
    );
}

// ===========================================================================
// Split-frequency RF stamping
// ===========================================================================

/// When rig split is active (split TX dial ≠ 0), a completed QSO must log the
/// **split TX dial + audio offset** as its RF frequency — NOT the RX dial.
///
/// Setup: RX dial = 14.074 MHz, split TX dial = 14.090 MHz, audio offset ≈ 1500 Hz.
/// Expected completed `metadata.frequency` ≈ 14_091_500 Hz (split dial + offset),
/// cleanly distinguishable from the RX-based value (14_075_500).
///
/// This test drives a real autonomous QSO to completion through the real
/// `QsoManager::process_message` path, with the split atomic actually injected
/// via `set_split_tx_frequency_source`, so `effective_tx_dial` runs for real.
#[tokio::test]
async fn split_active_qso_logs_tx_dial() {
    let mut sim = CoordSim::new("K5ARH").await;

    // Set RX dial = 14.074 MHz, split TX dial = 14.090 MHz.
    sim.set_rx_dial(14_074_000);
    sim.set_split_tx(14_090_000);

    // Audio offset ≈ 1500 Hz (the DX's decoded frequency in the passband).
    let audio_offset = 1500.0_f64;

    // Slot 0: open the auto QSO.
    let qso_id = sim
        .autonomous_pounce("VB7F", audio_offset, Some(SlotParity::Odd))
        .await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;
    sim.timeline.assert_keyed_for_qso(&qso_id);

    // Slot 1: DX sends us a report → engine auto-sends our R-report.
    sim.inject_decode("K5ARH VB7F -12", audio_offset).await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    // Slot 2: DX rogers with RR73 → QSO completes; our 73 keys.
    sim.inject_decode("K5ARH VB7F RR73", audio_offset).await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    // The QsoCompleted event must have been captured in `sim.completed`.
    assert!(
        !sim.completed.is_empty(),
        "expected at least one QsoCompleted, got none.\n{}",
        sim.timeline
    );

    let meta = &sim.completed[0];
    let logged_freq = meta.frequency;

    // With split active the logged RF must be stamped against the split TX dial
    // (14.090 MHz + ~1500 Hz ≈ 14_091_500), NOT the RX dial (14.074 MHz + ~1500 Hz ≈ 14_075_500).
    let split_tx_base = 14_090_000.0_f64;
    let rx_base = 14_074_000.0_f64;

    assert!(
        logged_freq >= split_tx_base && logged_freq < split_tx_base + 4_000.0,
        "expected RF stamped against split TX dial (~{split_tx_base}..+4kHz), \
         but got {logged_freq:.0} Hz (rx-based would be ~{rx_base:.0}+offset).\n{}",
        sim.timeline
    );
    // And confirm it is NOT the RX-dial-based value.
    assert!(
        (logged_freq - rx_base).abs() > 5_000.0,
        "RF frequency {logged_freq:.0} Hz is too close to the RX dial {rx_base:.0} Hz; \
         split TX dial was not used.\n{}",
        sim.timeline
    );
}

// ===========================================================================
// Hound mode — DXpedition chaser (Task 9, coord-level wire proof)
//
// The engine-level QSY is unit-tested in pancetta-qso/src/qso_manager.rs.
// This scenario proves it reaches the **modulator/PTT through the real
// coordinator path**: engage_hound → PTT keys at the low calling offset
// (300–900 Hz), then after the Fox's report the next slot keys at the high
// QSY'd offset (1000–2700 Hz) — offset-on-the-wire, not just state.
//
// Deterministic offset for seed "D2UY":
//   low  = 700 Hz  (hound_offset_for("D2UY", 300, 900))
//   high = 2140 Hz (hound_offset_for("D2UY", 1000, 2700))
// ===========================================================================

/// Hound engage keys PTT at the low calling offset (300–900 Hz) first, then
/// QSYs to the high response offset (1000–2700 Hz) after the Fox sends its
/// signal report — proved at the mock-rig PTT + keyed-audio-offset level
/// through the real coordinator TX path.
///
/// Also drives the Fox's RR73 to verify the QSO reaches `Completed`.
#[tokio::test]
async fn hound_engage_keys_low_then_qsys_high_on_report() {
    let mut sim = CoordSim::new("K5ARH").await;

    // ── Slot 0: engage Hound on Fox "D2UY" at 1800 Hz (Fox's audio offset) ──
    //
    // engage_hound picks our deterministic LOW calling offset (700 Hz for D2UY)
    // and emits the opening CqResponse (`D2UY K5ARH EM10`) there.
    let qso_id = sim
        .manager
        .engage_hound("D2UY", 1800.0, Some("JI64"), Some(SlotParity::Even))
        .await
        .expect("engage_hound must succeed");

    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "engage_hound must emit at least one TransmitRequest (the opening call)"
    );

    // Verify the forwarded offset is in the LOW calling region before keying.
    let opening_offset = pending[0].frequency_offset;
    assert!(
        (300.0..=900.0).contains(&opening_offset),
        "opening TransmitRequest must be in the Hound calling region [300, 900] Hz, got {opening_offset}"
    );

    let slot0 = sim.drive_slot(pending).await;

    // PTT must have keyed, and at the LOW calling offset.
    let qso_id_str = qso_id.to_string();
    sim.timeline.assert_keyed_for_qso(&qso_id_str);
    let keyed_slot0 = sim.timeline.keyed_for_qso(&qso_id_str);
    let low_freq = keyed_slot0
        .iter()
        .find(|k| k.slot == slot0 && k.ptt_keyed)
        .map(|k| k.freq_hz)
        .expect("a keyed TX must exist in slot 0");
    assert!(
        (300.0..=900.0).contains(&low_freq),
        "slot 0 must key in the Hound calling region [300, 900] Hz, got {low_freq} Hz.\n{}",
        sim.timeline
    );
    // Ideally == 700 Hz (deterministic for "D2UY").
    assert!(
        (low_freq - 700.0).abs() < 5.0,
        "slot 0 keyed offset should be 700 Hz (deterministic for D2UY), got {low_freq} Hz.\n{}",
        sim.timeline
    );

    // ── Slot 1: Fox sends signal report at its own offset (1800 Hz) → QSY ──
    //
    // inject_decode mirrors the coordinator's decode loop: parse → process_message.
    // The Fox's report ("K5ARH D2UY -12") is injected at 1800 Hz (Fox's audio
    // offset = partner_freq); the relevance gate routes it to the Hound QSO because
    // partner_freq == 1800.0, even though our TX is at 700 Hz.
    sim.inject_decode("K5ARH D2UY -12", 1800.0).await;

    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "Fox report must trigger a ReportAck TransmitRequest (QSY'd offset)"
    );

    // The ReportAck must be at the HIGH response offset (QSY'd), not the low one.
    let report_ack_offset = pending[0].frequency_offset;
    assert!(
        (1000.0..=2700.0).contains(&report_ack_offset),
        "ReportAck TransmitRequest must be in the QSY'd response region [1000, 2700] Hz, \
         got {report_ack_offset} Hz.\n{}",
        sim.timeline
    );

    let slot1 = sim.drive_slot(pending).await;

    // PTT must have keyed in slot 1 at the HIGH offset — this is the on-wire proof.
    let keyed_slot1 = sim.timeline.keyed_for_qso(&qso_id_str);
    let high_freq = keyed_slot1
        .iter()
        .find(|k| k.slot == slot1 && k.ptt_keyed)
        .map(|k| k.freq_hz)
        .expect("a keyed TX must exist in slot 1 (ReportAck after QSY)");
    assert!(
        (1000.0..=2700.0).contains(&high_freq),
        "slot 1 must key in the Hound response region [1000, 2700] Hz, got {high_freq} Hz.\n{}",
        sim.timeline
    );
    // Ideally == 2140 Hz (deterministic for "D2UY").
    assert!(
        (high_freq - 2140.0).abs() < 5.0,
        "slot 1 keyed offset should be 2140 Hz (deterministic QSY for D2UY), got {high_freq} Hz.\n{}",
        sim.timeline
    );
    // The high offset must be strictly higher than the low calling offset.
    assert!(
        high_freq > low_freq,
        "QSY'd offset {high_freq} Hz must be higher than calling offset {low_freq} Hz.\n{}",
        sim.timeline
    );

    // ── Slot 2 (optional completion leg): Fox sends RR73 → QSO completes ──
    //
    // inject at Fox's offset (1800 Hz); relevance gate accepts via partner_freq.
    sim.inject_decode("K5ARH D2UY RR73", 1800.0).await;

    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    // The QSO must have completed (QsoCompleted captured by pump_qso_events).
    // If the 73 was emitted and the QsoCompleted event fired, `sim.completed` is non-empty.
    assert!(
        !sim.completed.is_empty(),
        "QSO must reach Completed after Fox RR73 (QsoCompleted event expected).\n{}",
        sim.timeline
    );

    // All PTT transitions must be cleanly released.
    sim.timeline.assert_all_released();
}

// ===========================================================================
// TX-offset control (Task 5) — held-offset honored, multi-TX de-confliction,
// Tx=Rx regression. These prove that `compute_manual_tx_offset` (the pure
// coordinator-side selection logic) produces the right TX audio offset and that
// the chosen offset reaches the mock rig PTT wire.
//
// `compute_manual_tx_offset` is re-exported from `pancetta_lib::coordinator`
// (the same path `coalesce_transmit_requests` / `tx_qso_is_live` use) so the
// tests call the real function without duplicating the logic inline.
// ===========================================================================

/// **Held offset honored (single):** operator is in Hold mode with held=1500 Hz,
/// DX decoded at 700 Hz. `compute_manual_tx_offset` must choose tx_off=1500 and
/// partner_freq=Some(700) (we TX at the held spot; DX's replies are at 700).
///
/// The test:
/// 1. Calls `compute_manual_tx_offset` directly and asserts the selection.
/// 2. Opens the QSO with `respond_to_cq_with(dx=700, tx_off=1500, partner=Some(700))`.
/// 3. Drives a slot → asserts PTT keyed at 1500 Hz, NOT 700.
/// 4. Injects the DX reply at 700 Hz → asserts the QSO advances (relevance gate
///    routes via `partner_freq`).
#[tokio::test]
async fn held_offset_honored_keys_at_held_not_dx_freq() {
    use pancetta_lib::coordinator::compute_manual_tx_offset;

    // --- Step 1: selection-level proof ---
    let dx_freq = 700.0_f64;
    let held_hz: u64 = 1500;
    let (tx_off, partner) = compute_manual_tx_offset(dx_freq, true, held_hz, &[]);
    assert!(
        (tx_off - 1500.0).abs() < 1.0,
        "hold mode + held=1500 + no collision must yield tx_off=1500, got {tx_off}"
    );
    assert_eq!(
        partner,
        Some(700.0),
        "when tx_off ≠ dx_freq the partner_freq must be Some(dx_freq), got {partner:?}"
    );

    // --- Step 2: open the QSO with the computed offset + partner ---
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "VK4DX".to_string(),
            tx_off, // our TX offset (1500 Hz)
            Some(SlotParity::Even),
            CallInitiation::Manual,
            partner, // DX's decode freq (700 Hz)
            false,
        )
        .await
        .expect("respond_to_cq_with with held offset");

    // --- Step 3: drive a slot → PTT must key at 1500 Hz, NOT at 700 Hz ---
    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "QSO open must emit at least one TransmitRequest"
    );

    // The forwarded request must carry the held offset, not the DX's freq.
    assert!(
        pending
            .iter()
            .any(|p| (p.frequency_offset - 1500.0).abs() < 1.0),
        "forwarded TransmitRequest must carry the held offset 1500 Hz, got {:?}",
        pending
            .iter()
            .map(|p| p.frequency_offset)
            .collect::<Vec<_>>()
    );
    assert!(
        !pending
            .iter()
            .any(|p| (p.frequency_offset - 700.0).abs() < 1.0),
        "no TransmitRequest must carry the DX freq 700 Hz (we TX at the held spot)"
    );

    sim.drive_slot(pending).await;

    let qso_id_str = qso_id.to_string();
    sim.timeline.assert_keyed_for_qso(&qso_id_str);
    sim.timeline.assert_keyed_at_offset(&qso_id_str, 1500.0);

    // Confirm we never accidentally keyed at the DX's decoded frequency.
    let keyed = sim.timeline.keyed_for_qso(&qso_id_str);
    assert!(
        keyed.iter().all(|k| (k.freq_hz - 700.0).abs() > 1.0),
        "PTT must NEVER key at the DX decoded freq 700 Hz (we are holding 1500 Hz).\n{}",
        sim.timeline
    );

    // --- Step 4: DX replies at 700 Hz → relevance gate via partner_freq routes it ---
    // The QSO is currently in RespondingToCq (we sent our grid); DX sends us a
    // report at 700 Hz (the DX's own audio position). The relevance gate uses
    // partner_freq=700 to match it to our QSO even though our TX is at 1500.
    sim.inject_decode("K5ARH VK4DX -15", 700.0).await;
    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "DX report injected at 700 Hz (partner_freq) must advance QSO and emit a reply.\n{}",
        sim.timeline
    );

    // The follow-on reply must still be at our held offset (1500 Hz), not 700.
    assert!(
        pending
            .iter()
            .any(|p| (p.frequency_offset - 1500.0).abs() < 1.0),
        "follow-on TransmitRequest after DX reply must still carry 1500 Hz, got {:?}",
        pending
            .iter()
            .map(|p| p.frequency_offset)
            .collect::<Vec<_>>()
    );

    sim.timeline.assert_all_released();
}

/// **Multi-TX de-confliction:** opening a second QSO whose DX offset (1540 Hz)
/// lands within `MIN_TX_SEPARATION_HZ` (75 Hz) of an already-active QSO's TX
/// offset (1500 Hz) must produce a de-conflicted TX offset ≥ 75 Hz away, so
/// the two streams do NOT collide in the audio passband.
///
/// The test:
/// 1. Opens QSO A on 1500 Hz (Auto, no held), drives its opening slot → A keys
///    at 1500 Hz.
/// 2. Calls `compute_manual_tx_offset` with `active_tx_offsets()` = [1500] and
///    candidate DX = 1540 Hz (within 75 Hz of A) → confirms de-confliction.
/// 3. Opens QSO B at the de-conflicted offset (≥75 Hz from 1500).
/// 4. Drives B's opening slot → B keys at the de-conflicted offset.
/// 5. Asserts both QSOs keyed on distinct offsets ≥75 Hz apart (inspecting the
///    full timeline across both slots).
#[tokio::test]
async fn multi_tx_deconfliction_offsets_are_distinct() {
    use pancetta_lib::coordinator::compute_manual_tx_offset;
    use pancetta_qso::MIN_TX_SEPARATION_HZ;

    let mut sim = CoordSim::new("K5ARH").await;

    // --- Slot 0: open QSO A at 1500 Hz, drive its opening ---
    let a = sim
        .manager
        .respond_to_cq_with(
            "EA8DX".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
        )
        .await
        .expect("qso A");
    let pending_a = sim.pump_qso_events();
    assert!(
        !pending_a.is_empty(),
        "QSO A must forward at least one TransmitRequest"
    );
    sim.drive_slot(pending_a).await;
    sim.timeline.assert_keyed_for_qso(&a.to_string());
    sim.timeline.assert_keyed_at_offset(&a.to_string(), 1500.0);

    // --- Compute de-conflicted offset for QSO B ---
    // QSO B's DX is at 1540 Hz (38 Hz from A — well within MIN_TX_SEPARATION_HZ).
    // active_tx_offsets must include A's 1500 Hz so the de-confliction fires.
    let active = sim.manager.active_tx_offsets().await;
    assert!(
        active.iter().any(|&o| (o - 1500.0).abs() < 1.0),
        "active_tx_offsets must include QSO A's 1500 Hz offset, got {active:?}"
    );

    let dx_b = 1540.0_f64;
    let (tx_off_b, _partner_b) = compute_manual_tx_offset(dx_b, false, 0, &active);

    // The de-conflicted result must be ≥75 Hz from 1500 Hz.
    let sep = (tx_off_b - 1500.0_f64).abs();
    assert!(
        sep >= MIN_TX_SEPARATION_HZ,
        "de-conflicted offset {tx_off_b} Hz must be ≥{MIN_TX_SEPARATION_HZ} Hz from A's 1500 Hz (sep={sep:.1} Hz)"
    );

    // --- Slot 1: open QSO B at the de-conflicted offset, drive its opening ---
    let b = sim
        .manager
        .respond_to_cq_with(
            "UA9XYZ".to_string(),
            tx_off_b,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
        )
        .await
        .expect("qso B");

    let pending_b = sim.pump_qso_events();
    assert!(
        !pending_b.is_empty(),
        "QSO B must forward at least one TransmitRequest"
    );
    let slot_b = sim.drive_slot(pending_b).await;

    // B must have keyed PTT at the de-conflicted offset.
    sim.timeline.assert_keyed_for_qso(&b.to_string());
    sim.timeline
        .assert_keyed_at_offset(&b.to_string(), tx_off_b);

    // The de-conflicted offset must also appear in slot_b (B's opening slot).
    let b_offsets = sim.timeline.offsets_in_slot(slot_b);
    assert!(
        b_offsets.iter().any(|&o| (o - tx_off_b).abs() < 1.0),
        "B's opening slot must show the de-conflicted offset {tx_off_b:.0} Hz, got {b_offsets:?}.\n{}",
        sim.timeline
    );

    // Over the full timeline: A keyed at 1500, B at a distinct de-conflicted offset ≥75 Hz away.
    let a_keyed = sim.timeline.keyed_for_qso(&a.to_string());
    let b_keyed = sim.timeline.keyed_for_qso(&b.to_string());
    let a_off = a_keyed
        .iter()
        .filter(|k| k.ptt_keyed)
        .map(|k| k.freq_hz)
        .next()
        .expect("QSO A must have keyed PTT at least once");
    let b_off = b_keyed
        .iter()
        .filter(|k| k.ptt_keyed)
        .map(|k| k.freq_hz)
        .next()
        .expect("QSO B must have keyed PTT at least once");

    assert!(
        (a_off - 1500.0).abs() < 1.0,
        "QSO A must key at 1500 Hz, got {a_off} Hz.\n{}",
        sim.timeline
    );
    let actual_sep = (b_off - a_off).abs();
    assert!(
        actual_sep >= MIN_TX_SEPARATION_HZ,
        "QSO A keyed at {a_off:.0} Hz and QSO B at {b_off:.0} Hz — separation {actual_sep:.1} Hz \
         must be ≥{MIN_TX_SEPARATION_HZ} Hz.\n{}",
        sim.timeline
    );

    // Both PTT transitions must be cleanly released.
    sim.timeline.assert_all_released();
}

/// **Regression — Auto, single, no collision → Tx=Rx, partner_freq=None:**
/// with `TxFreqMode::Auto` (hold_mode=false) and no already-active QSOs,
/// `compute_manual_tx_offset` must return the DX's own freq with `partner_freq=None`
/// — the byte-identical "Tx=Rx" behavior from before this feature was added.
/// Opening the QSO must key at the DX's freq (no silent shift), and the DX's
/// reply at that same freq must route without needing `partner_freq`.
#[tokio::test]
async fn auto_single_no_collision_is_tx_eq_rx() {
    use pancetta_lib::coordinator::compute_manual_tx_offset;

    // --- Selection-level regression: Auto, no held, no collision → Tx=Rx ---
    let dx_freq = 1500.0_f64;
    let (tx_off, partner) = compute_manual_tx_offset(dx_freq, false, 0, &[]);
    assert!(
        (tx_off - dx_freq).abs() < 1.0,
        "Auto + no-held + no-collision must yield tx_off == dx_freq, got {tx_off}"
    );
    assert_eq!(
        partner, None,
        "Auto + no-held + no-collision must yield partner_freq=None (Tx=Rx), got {partner:?}"
    );

    // --- Rig-level regression: open with the computed values, must key at dx_freq ---
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "JH1XYZ".to_string(),
            tx_off, // == dx_freq == 1500.0
            Some(SlotParity::Odd),
            CallInitiation::Auto,
            partner, // None — Tx=Rx, no partner_freq split
            false,
        )
        .await
        .expect("respond_to_cq_with Tx=Rx");

    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "Tx=Rx QSO open must emit a TransmitRequest"
    );

    // Forwarded request must carry 1500 Hz.
    assert!(
        pending
            .iter()
            .any(|p| (p.frequency_offset - 1500.0).abs() < 1.0),
        "forwarded request must carry 1500 Hz (Tx=Rx), got {:?}",
        pending
            .iter()
            .map(|p| p.frequency_offset)
            .collect::<Vec<_>>()
    );

    sim.drive_slot(pending).await;

    let qso_id_str = qso_id.to_string();
    sim.timeline.assert_keyed_for_qso(&qso_id_str);
    sim.timeline.assert_keyed_at_offset(&qso_id_str, 1500.0);

    // DX replies at 1500 Hz (same freq, no partner split needed) → QSO advances.
    sim.inject_decode("K5ARH JH1XYZ -09", 1500.0).await;
    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "DX reply at 1500 Hz must advance Tx=Rx QSO (no partner_freq needed).\n{}",
        sim.timeline
    );

    // Follow-on reply must still be at 1500 Hz (TX offset unchanged).
    assert!(
        pending
            .iter()
            .any(|p| (p.frequency_offset - 1500.0).abs() < 1.0),
        "follow-on reply must be at 1500 Hz (Tx=Rx regression), got {:?}",
        pending
            .iter()
            .map(|p| p.frequency_offset)
            .collect::<Vec<_>>()
    );

    sim.timeline.assert_all_released();
}

// ===========================================================================
// Fox mode — DXpedition multi-stream rig-level proof (Task 3)
//
// Fox answers many Hound callers concurrently. Each answer is a normal
// `respond_to_caller(ResponseStep::Grid)` QSO (same path `maybe_answer_caller`
// uses internally), all on the SAME parity so the coalescer folds them into one
// `MultiTransmitRequest` and keys BOTH in a single 15-second slot.
//
// Offset de-confliction is exercised via `compute_manual_tx_offset` with the
// live `active_tx_offsets()` snapshot, exactly as the coordinator's caller-
// answer handler does it. The test proves:
//   • ≥2 distinct offsets keyed in ONE slot
//   • Every pair of keyed offsets is ≥ MIN_TX_SEPARATION_HZ (75 Hz) apart
//   • Each QSO advances when injected with its Hound's R-report (sequencing
//     works for concurrent Fox QSOs)
//
// Why `respond_to_caller` × 2, not `maybe_answer_caller`:
//   `maybe_answer_caller` is a private `async fn` on the coordinator struct
//   (not accessible from the test binary). `respond_to_caller` with
//   `ResponseStep::Grid` is the exact call it makes internally
//   (qso_manager.rs:1259-1268); the test is therefore equivalent in fidelity
//   to calling the production path. The cap-admits-N / rejects-N+1 logic is
//   separately unit-tested in `pancetta/tests/fox_mode.rs::fox_cap_admits_n_and_rejects_n_plus_1`.
// ===========================================================================

/// Fox mode: two Hound callers answered in ONE slot, multi-streamed on distinct
/// de-conflicted offsets (≥ 75 Hz apart), both advancing toward RR73.
///
/// This is the rig-level multi-stream proof: the mock rig observes TWO distinct
/// audio offsets keyed in a single slot — exactly what a DXpedition Fox needs.
#[tokio::test]
async fn fox_mode_answers_two_callers_multistreamed_distinct_offsets() {
    use pancetta_lib::coordinator::compute_manual_tx_offset;
    use pancetta_qso::MIN_TX_SEPARATION_HZ;

    let mut sim = CoordSim::new("K5ARH").await;

    // ── Step 1: Fox answers Hound A ──────────────────────────────────────────
    //
    // Hound A is calling at 1200 Hz (the Fox hears their CQ/call there).
    // Auto, no held offset: `compute_manual_tx_offset` returns Tx=Rx (1200 Hz,
    // no collision yet).
    let hound_a_dx_freq = 1200.0_f64;
    let (tx_off_a, partner_a) = compute_manual_tx_offset(hound_a_dx_freq, false, 0, &[]);

    let qso_a = sim
        .manager
        .respond_to_caller(
            "W1AAA".to_string(),
            tx_off_a,
            Some(SlotParity::Even),
            pancetta_core::ResponseStep::Grid,
            Some(-10.0),
            None,
            partner_a,
            false,
        )
        .await
        .expect("Fox must be able to answer Hound A");

    // ── Step 2: Fox answers Hound B ──────────────────────────────────────────
    //
    // Hound B is calling at 1240 Hz — only 40 Hz from A, within the 75 Hz
    // minimum separation. The coordinator fetches `active_tx_offsets` (which
    // now includes A's offset) and calls `compute_manual_tx_offset` to get a
    // de-conflicted TX offset for B.
    let active = sim.manager.active_tx_offsets().await;
    assert!(
        active.iter().any(|&o| (o - tx_off_a).abs() < 1.0),
        "active_tx_offsets must include Hound A's offset {tx_off_a:.0} Hz after opening, got {active:?}"
    );

    let hound_b_dx_freq = 1240.0_f64; // within 75 Hz of A → triggers de-confliction
    let (tx_off_b, partner_b) = compute_manual_tx_offset(hound_b_dx_freq, false, 0, &active);

    // Sanity-check: de-conflicted offset must be ≥ MIN_TX_SEPARATION_HZ from A.
    let sep = (tx_off_b - tx_off_a).abs();
    assert!(
        sep >= MIN_TX_SEPARATION_HZ,
        "de-conflicted Hound B offset {tx_off_b:.0} Hz must be ≥{MIN_TX_SEPARATION_HZ} Hz from \
         Hound A offset {tx_off_a:.0} Hz (sep={sep:.1} Hz)"
    );

    let qso_b = sim
        .manager
        .respond_to_caller(
            "W2BBB".to_string(),
            tx_off_b,
            Some(SlotParity::Even),
            pancetta_core::ResponseStep::Grid,
            Some(-12.0),
            None,
            partner_b,
            false,
        )
        .await
        .expect("Fox must be able to answer Hound B");

    // ── Step 3: Pump events + drive one slot ─────────────────────────────────
    //
    // Both QSOs go active (StateChanged → RespondingToCq), both forward their
    // opening TransmitRequest (our grid reply to each Hound). The coalescer
    // keeps BOTH (distinct qso_ids, same parity) and the TX worker keys them
    // sequentially within the slot. The mock rig therefore observes TWO PTT
    // pulses at TWO distinct offsets.
    let pending = sim.pump_qso_events();
    assert!(
        pending.len() >= 2,
        "Fox must forward ≥2 TransmitRequests (one per Hound) after opening both QSOs, got {}",
        pending.len()
    );

    let slot0 = sim.drive_slot(pending).await;

    // Both QSOs must have keyed PTT.
    let qso_a_str = qso_a.to_string();
    let qso_b_str = qso_b.to_string();
    sim.timeline.assert_keyed_for_qso(&qso_a_str);
    sim.timeline.assert_keyed_for_qso(&qso_b_str);

    // Each keyed at its de-conflicted offset (on-wire proof).
    sim.timeline.assert_keyed_at_offset(&qso_a_str, tx_off_a);
    sim.timeline.assert_keyed_at_offset(&qso_b_str, tx_off_b);

    // TWO distinct offsets in ONE slot — the core multi-stream assertion.
    sim.timeline.assert_distinct_offsets_in_slot(slot0, 2);

    // The two offsets must be ≥ MIN_TX_SEPARATION_HZ apart (no on-air collision).
    let mut offsets_slot0 = sim.timeline.offsets_in_slot(slot0);
    offsets_slot0.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(
        offsets_slot0.len(),
        2,
        "expected exactly 2 keyed offsets in slot 0"
    );
    let actual_sep = (offsets_slot0[0] - offsets_slot0[1]).abs();
    assert!(
        actual_sep >= MIN_TX_SEPARATION_HZ,
        "Fox streams must be ≥{MIN_TX_SEPARATION_HZ} Hz apart on the wire; \
         got {:.0} Hz and {:.0} Hz (sep={:.1} Hz).\n{}",
        offsets_slot0[0],
        offsets_slot0[1],
        actual_sep,
        sim.timeline
    );

    // ── Step 4: Inject each Hound's R-report → QSOs sequence forward ─────────
    //
    // The Fox receives Hound A's report at A's own audio position (hound_a_dx_freq
    // if Tx=Rx; or via partner_freq when de-conflicted). Both QSOs must advance
    // to the ReportAck rung, proving concurrent Fox sequencing works.
    //
    // Inject at the DX's decode frequency (where the Hound's signal lives), which
    // is hound_a_dx_freq / hound_b_dx_freq. The relevance gate uses partner_freq
    // when tx_off ≠ dx_freq, so the report routes correctly even when de-conflicted.
    sim.inject_decode("K5ARH W1AAA -10", hound_a_dx_freq).await;
    sim.inject_decode("K5ARH W2BBB -12", hound_b_dx_freq).await;

    let pending_s1 = sim.pump_qso_events();
    assert!(
        !pending_s1.is_empty(),
        "Hound R-reports must trigger ReportAck replies from Fox; got no pending TX.\n{}",
        sim.timeline
    );

    let _slot1 = sim.drive_slot(pending_s1).await;

    // Both QSOs must still be keying (ReportAck rung, sequencing works).
    let a_keyed = sim.timeline.keyed_for_qso(&qso_a_str);
    let b_keyed = sim.timeline.keyed_for_qso(&qso_b_str);
    assert!(
        a_keyed.len() >= 2,
        "Fox QSO A must have keyed ≥2 times (grid + ReportAck), got {}.\n{}",
        a_keyed.len(),
        sim.timeline
    );
    assert!(
        b_keyed.len() >= 2,
        "Fox QSO B must have keyed ≥2 times (grid + ReportAck), got {}.\n{}",
        b_keyed.len(),
        sim.timeline
    );

    // All PTT transitions cleanly released (no stuck PTT).
    sim.timeline.assert_all_released();
}

// ===========================================================================
// Station-agent remote-TX arm gate (P2.3). These exercise the ONE change to
// the live coordinator TX path: TxOrigin::Remote requests are gated by the
// shared ArmState before keying PTT; Local requests are byte-identical.
// ===========================================================================

/// A remote-origin request with a FRESH (unarmed) arm is dropped — no PTT.
#[tokio::test]
async fn remote_tx_dropped_when_arm_fresh_unarmed() {
    let mut sim = CoordSim::new("K5ARH").await;
    // Arm is fresh/unarmed by construction → tx_permitted() is false.
    let remote = sim.remote_tx("CQ K5ARH EM10", 1500.0);
    sim.drive_slot(vec![remote]).await;

    sim.timeline.assert_silent();
    assert!(
        sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::RemoteNotArmed),
        "expected a RemoteNotArmed drop.\n{}",
        sim.timeline
    );
}

/// A LOCAL-origin request keys PTT regardless of the arm state — byte-identical
/// to the pre-gate path (the arm gate never applies to Local).
#[tokio::test]
async fn local_tx_unaffected_by_remote_arm() {
    let mut sim = CoordSim::new("K5ARH").await;
    // Arm stays fresh/unarmed; a Local manual send must still key.
    let local = sim.manual_tx("CQ K5ARH EM10", 1500.0);
    sim.drive_slot(vec![local]).await;

    assert!(
        sim.timeline.keyed_anything(),
        "a Local request must key PTT even with an unarmed remote arm.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
    assert!(
        !sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::RemoteNotArmed),
        "a Local request must never be dropped by the remote arm gate.\n{}",
        sim.timeline
    );
}

/// With a fully-armed + consented + heartbeat-fresh arm, a Remote request keys
/// PTT (the gate opens).
#[tokio::test]
async fn remote_tx_keys_when_armed_and_consented() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.arm_remote_tx();
    let remote = sim.remote_tx("CQ K5ARH EM10", 1500.0);
    sim.drive_slot(vec![remote]).await;

    assert!(
        sim.timeline.keyed_anything(),
        "a Remote request must key PTT when the arm is armed + consented.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
}

/// Policy primacy: `TxPolicy::Disabled` drops even a Remote request that has a
/// live arm — the hard-mute runs first.
#[tokio::test]
async fn policy_disabled_drops_remote_tx_even_with_live_arm() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.arm_remote_tx(); // arm would otherwise permit
    sim.set_policy(TxPolicy::Disabled);
    let remote = sim.remote_tx("CQ K5ARH EM10", 1500.0);
    sim.drive_slot(vec![remote]).await;

    sim.timeline.assert_silent();
    assert!(
        sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::PolicyDisabled),
        "Disabled policy must drop the remote request first (policy primacy).\n{}",
        sim.timeline
    );
}

// ============================================================================
// B2 (P3.4c) — end-to-end: a REAL remote-origin QSO's TX is arm-gated.
//
// These drive the TX through the actual QSO engine (a `remote_origin=true`
// QSO's `MessageToSend`), pumped through the faithful `pump_qso_events`
// origin-derivation mirror, so they prove the whole chain
//   QsoMetadata.remote_origin → MessageToSend → PendingTx.origin=Remote → arm gate
// rather than a synthetic `remote_tx()` PendingTx.
// ============================================================================

/// A remote-origin QSO created but NOT armed: its opening TX is dropped (no PTT).
#[tokio::test]
async fn remote_origin_qso_tx_dropped_when_unarmed() {
    let mut sim = CoordSim::new("K5ARH").await;
    // Arm is fresh/unarmed. Open a remote-origin QSO through the real engine.
    sim.manager
        .respond_to_cq_with(
            "W1XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true, // remote_origin
        )
        .await
        .expect("respond_to_cq_with");

    let pending = sim.pump_qso_events();
    // The forwarded frame MUST be Remote (else it would bypass the arm).
    assert!(
        pending
            .iter()
            .any(|p| p.origin == pancetta_lib::message_bus::TxOrigin::Remote),
        "a remote_origin QSO's forwarded TX must be TxOrigin::Remote"
    );
    sim.drive_slot(pending).await;

    // Unarmed → the whole slot is silent, dropped by the arm gate.
    sim.timeline.assert_silent();
    assert!(
        sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::RemoteNotArmed),
        "an unarmed remote QSO's TX must be dropped by the arm gate (no bypass).\n{}",
        sim.timeline
    );
}

/// The SAME remote-origin QSO keys PTT once the arm is armed + consented.
#[tokio::test]
async fn remote_origin_qso_tx_keys_when_armed() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.arm_remote_tx();
    sim.manager
        .respond_to_cq_with(
            "W1XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true, // remote_origin
        )
        .await
        .expect("respond_to_cq_with");

    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    assert!(
        sim.timeline.keyed_anything(),
        "an armed remote QSO's TX must key PTT.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
}

/// Regression: a LOCAL QSO (remote_origin=false) keys PTT regardless of the
/// unarmed remote arm — byte-identical to the pre-P3.4c path.
#[tokio::test]
async fn local_origin_qso_tx_unaffected_by_arm() {
    let mut sim = CoordSim::new("K5ARH").await;
    // Arm stays unarmed; a local QSO must still key.
    sim.manager
        .respond_to_cq_with(
            "W1XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            false, // local
        )
        .await
        .expect("respond_to_cq_with");

    let pending = sim.pump_qso_events();
    assert!(
        pending
            .iter()
            .all(|p| p.origin == pancetta_lib::message_bus::TxOrigin::Local),
        "a local QSO's forwarded TX must be TxOrigin::Local (regression)"
    );
    sim.drive_slot(pending).await;

    assert!(
        sim.timeline.keyed_anything(),
        "a local QSO's TX must key PTT even with an unarmed remote arm.\n{}",
        sim.timeline
    );
    assert!(
        !sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::RemoteNotArmed),
        "a local QSO must never be dropped by the remote arm gate.\n{}",
        sim.timeline
    );
}
