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
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use pancetta_hamlib::{MockRig, PttState, RigControl, Vfo};
use pancetta_lib::coordinator::{
    active_tx_qso_key, coalesce_transmit_requests, tx_qso_is_live, CoalesceEntry,
};
use pancetta_lib::message_bus::{
    ComponentId, ComponentMessage, MessageBus, MessageType, RigControlMessage,
};
use pancetta_qso::{CallInitiation, QsoEvent, QsoManager, QsoManagerConfig, QsoState};

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
        let manager = QsoManager::new(config);
        let qso_rx = manager.subscribe();

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
        }
    }

    /// Set the global TX policy (mirrors the operator's `g` cycle / Shift+Q).
    pub fn set_policy(&self, policy: TxPolicy) {
        self.tx_policy.store(policy.as_u8(), Ordering::Release);
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
            QsoEvent::QsoCompleted { qso_id, .. } => {
                // Grace window: keep the key live so the final 73 still keys.
                // The scenario removes it explicitly via `expire_qso` when it
                // wants to model grace elapsing.
                let key = active_tx_qso_key(&qso_id.to_string());
                self.active_tx_qsos.write().unwrap().insert(key);
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
            } => {
                // The coordinator renders the QsoMessage to FT8 text via
                // pancetta_qso::utils::generate_ft8_message. We carry the raw
                // message debug for the timeline; the keying decision doesn't
                // depend on the rendered text, only on qso_id / freq / policy /
                // active-set, so a stable label is sufficient and avoids
                // re-deriving the (private-ish) text rules here.
                let text = render_message(&message, &self.our_callsign);
                pending.push(PendingTx {
                    text,
                    frequency_offset: frequency,
                    qso_id: Some(qso_id.to_string()),
                    tx_parity,
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

        // --- Coalesce backlog with the REAL production coalescer ---
        let drained: Vec<CoalesceEntry> = pending
            .iter()
            .map(|p| CoalesceEntry {
                message_text: p.text.clone(),
                frequency_offset: p.frequency_offset,
                qso_id: p.qso_id.clone(),
                tx_parity: p.tx_parity,
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
        },
        PendingTx {
            text: "KEEPCALL-OLD-2".to_string(),
            frequency_offset: 1800.0,
            qso_id: Some(id.clone()),
            tx_parity: Some(SlotParity::Even),
        },
        PendingTx {
            text: "KEEPCALL-NEWEST".to_string(),
            frequency_offset: 1800.0,
            qso_id: Some(id.clone()),
            tx_parity: Some(SlotParity::Even),
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
