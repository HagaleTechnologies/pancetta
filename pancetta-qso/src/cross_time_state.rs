//! Shared cross-slot state container — substrate for hb-048 / hb-057 / hb-173.
//!
//! # Purpose
//!
//! Three independent decoder-side hypotheses each need coordinator-level
//! cross-slot state shared across decoder threads:
//!
//! | hypothesis | needs | inner table |
//! |---|---|---|
//! | hb-057 (median-DT-per-callsign) | rolling DT sightings per callsign | [`CallsignDtHistory`] |
//! | hb-048 (a7 cross-correlation)   | "calls decoded last slot at audio freq f" | [`A7RecentCallTable`] |
//! | hb-173 (within-QSO context)     | active QSO state per callsign pair | [`WithinQsoContext`] |
//!
//! Rather than ship three parallel `Arc<RwLock<...>>` plumbings through the
//! coordinator, this module unifies the *bookkeeping* into a single
//! [`CrossTimeState`] container. Each inner table evolves independently;
//! the wrapper just gives the coordinator one lifetime to manage and one
//! place to instrument cold-start behavior.
//!
//! # Pattern: "first-to-graduate introduces, others fold in"
//!
//! hb-173's design spec (§"Shared infrastructure: `CrossTimeState`
//! crate-level handle") calls out that whichever of the three hypotheses
//! graduates first should introduce the container, and the other two fold
//! their tables in as fields. This crate ships the container *as
//! infrastructure* ahead of any of the three graduating, so the downstream
//! hypotheses can hook in via `cross_time.dt_history()`,
//! `cross_time.a7_recent_calls()`, and `cross_time.within_qso()` without
//! re-plumbing the coordinator each time.
//!
//! Per the Phase A audit conventions, this is a **SHIPPED-INFRA** module:
//! it changes no production decoding behavior. The tables are populated by
//! [`CrossTimeState::record_decode`] but no consumer reads them until a
//! downstream hypothesis adds the read path.
//!
//! # Thread-safety
//!
//! The expected access pattern is:
//!
//! - **Writes** are infrequent and originate from the coordinator's FT8
//!   thread after successful decodes pass the FP filter (≤100 per 15-second
//!   slot under heavy traffic).
//! - **Reads** are common: per decoder candidate / per sync-search step.
//!
//! Each inner table is wrapped in its own [`std::sync::RwLock`] so a read
//! on one table never blocks a write on another. The outer [`CrossTimeState`]
//! is shared via `Arc`; clone the `Arc` into each decoder thread.
//!
//! # Memory bounds
//!
//! All three tables are bounded by:
//!
//! 1. **Per-table capacity** (LRU-style eviction by `last_seen` once the
//!    cap is reached — implemented per-table; see each struct's docs).
//! 2. **Time-based eviction** (configurable `max_age` per table; e.g.
//!    30 min for [`CallsignDtHistory`], 90 s for [`WithinQsoContext`],
//!    30 s for [`A7RecentCallTable`]).
//!
//! Eviction is "lazy": every successful [`record_decode`](
//! CrossTimeState::record_decode) call also evicts expired entries from
//! the table it touches. Background sweeps can be added later if the lazy
//! path proves insufficient under sparse-traffic regimes.
//!
//! # See also
//!
//! - `docs/superpowers/specs/2026-05-31-hb-057-median-dt-design.md`
//! - `docs/superpowers/specs/2026-05-31-hb-048-a7-design.md`
//! - `docs/superpowers/specs/2026-06-01-hb-173-within-qso-design.md`
//! - [`crate::autonomous::AutonomousOperator::recently_responded_to`] —
//!   analog per-callsign memory on the TX side (60 s window).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

/// Default per-callsign capacity for DT history ring buffer.
///
/// JTDX uses 5; pancetta doubles it because 24/7 ops produce more
/// sightings than typical-operator sessions. The diagnostic's
/// "moderate variance" bucket is dominated by callsigns with 3-5
/// sightings, so 10 is comfortably above the statistically-meaningful
/// floor.
pub const DEFAULT_DT_HISTORY_CAPACITY: usize = 10;

/// Default per-callsign max age for DT history (30 min). Matches the
/// callsign-continuity rolling window.
pub const DEFAULT_DT_HISTORY_MAX_AGE: Duration = Duration::from_secs(30 * 60);

/// Default global capacity for the DT history table (max distinct callsigns).
pub const DEFAULT_DT_HISTORY_MAX_CALLSIGNS: usize = 1024;

/// Default max age for a7 recent-call entries (30 s = 2 slots).
pub const DEFAULT_A7_MAX_AGE: Duration = Duration::from_secs(30);

/// Default capacity (max entries) for the a7 recent-call table.
pub const DEFAULT_A7_CAPACITY: usize = 256;

/// Default max age for within-QSO entries (90 s = 6 slots). See hb-173 spec.
pub const DEFAULT_WITHIN_QSO_MAX_AGE: Duration = Duration::from_secs(90);

/// Default capacity (max entries) for the within-QSO context table.
pub const DEFAULT_WITHIN_QSO_CAPACITY: usize = 256;

// -- DT history (hb-057) ----------------------------------------------------

/// A single DT sighting for [`CallsignDtHistory`].
#[derive(Clone, Copy, Debug)]
pub struct DtSighting {
    /// Wall-clock decode time; used for age-based eviction.
    pub at: SystemTime,
    /// `time_offset` (DT in seconds, slot-relative) of the decoded message.
    pub dt_s: f64,
}

/// Median-DT prior derived from a callsign's [`CallsignDtHistory`].
///
/// Returned by [`CallsignDtHistory::prior`] when at least
/// [`CallsignDtHistory::min_sightings`] sightings exist. Used by
/// hb-057 to narrow the time-axis of the localized Costas sync search.
#[derive(Clone, Copy, Debug)]
pub struct DtPrior {
    /// Median DT across all current sightings for this callsign.
    pub median_dt: f64,
    /// Inter-quartile range (P75 - P25). Used to widen the prior gate
    /// when variance is higher.
    pub iqr: f64,
    /// Number of sightings the median was computed from.
    pub sighting_count: usize,
}

/// Per-callsign rolling DT history with median + IQR statistics.
///
/// Capacity is bounded by `capacity` per callsign (default 10) and
/// by `max_callsigns` total (default 1024). Sightings older than `max_age`
/// (default 30 min) are evicted lazily on each [`record`](Self::record).
///
/// See `docs/superpowers/specs/2026-05-31-hb-057-median-dt-design.md`.
#[derive(Debug)]
pub struct CallsignDtHistory {
    entries: HashMap<String, VecDeque<DtSighting>>,
    /// LRU bookkeeping: most-recent-touch wall-clock per callsign.
    last_touch: HashMap<String, SystemTime>,
    /// Max age of a sighting before it expires.
    max_age: Duration,
    /// Per-callsign capacity (ring buffer length).
    capacity: usize,
    /// Max distinct callsigns; LRU-evict beyond this.
    max_callsigns: usize,
    /// Minimum sightings before [`prior`](Self::prior) returns Some.
    min_sightings: usize,
}

impl Default for CallsignDtHistory {
    fn default() -> Self {
        Self::new(
            DEFAULT_DT_HISTORY_CAPACITY,
            DEFAULT_DT_HISTORY_MAX_AGE,
            DEFAULT_DT_HISTORY_MAX_CALLSIGNS,
        )
    }
}

impl CallsignDtHistory {
    /// Create a new DT history table.
    pub fn new(capacity: usize, max_age: Duration, max_callsigns: usize) -> Self {
        Self {
            entries: HashMap::new(),
            last_touch: HashMap::new(),
            max_age,
            capacity,
            max_callsigns,
            min_sightings: 2,
        }
    }

    /// Override the minimum-sightings threshold (default 2).
    pub fn with_min_sightings(mut self, min: usize) -> Self {
        self.min_sightings = min;
        self
    }

    /// Record a new sighting. Evicts expired sightings for this callsign
    /// and (if at the global cap) LRU-evicts other callsigns.
    pub fn record(&mut self, callsign: &str, dt_s: f64, at: SystemTime) {
        self.evict_expired_for(callsign, at);
        let entry = self.entries.entry(callsign.to_string()).or_default();
        if entry.len() == self.capacity {
            entry.pop_front();
        }
        entry.push_back(DtSighting { at, dt_s });
        self.last_touch.insert(callsign.to_string(), at);
        self.maybe_lru_evict();
    }

    fn evict_expired_for(&mut self, callsign: &str, now: SystemTime) {
        if let Some(deque) = self.entries.get_mut(callsign) {
            while let Some(front) = deque.front() {
                match now.duration_since(front.at) {
                    Ok(age) if age > self.max_age => {
                        deque.pop_front();
                    }
                    _ => break,
                }
            }
            if deque.is_empty() {
                self.entries.remove(callsign);
                self.last_touch.remove(callsign);
            }
        }
    }

    /// Evict all sightings older than `max_age` across all callsigns. Idempotent.
    pub fn evict_expired(&mut self, now: SystemTime) {
        let callsigns: Vec<String> = self.entries.keys().cloned().collect();
        for callsign in callsigns {
            self.evict_expired_for(&callsign, now);
        }
    }

    fn maybe_lru_evict(&mut self) {
        while self.entries.len() > self.max_callsigns {
            // Find oldest by last_touch and drop it.
            if let Some((victim, _)) = self
                .last_touch
                .iter()
                .min_by_key(|(_, t)| **t)
                .map(|(k, v)| (k.clone(), *v))
            {
                self.entries.remove(&victim);
                self.last_touch.remove(&victim);
            } else {
                break;
            }
        }
    }

    /// Return the DT prior for `callsign` or `None` if fewer than
    /// `min_sightings` non-expired sightings exist.
    pub fn prior(&self, callsign: &str) -> Option<DtPrior> {
        let entries = self.entries.get(callsign)?;
        if entries.len() < self.min_sightings {
            return None;
        }
        let mut dts: Vec<f64> = entries.iter().map(|s| s.dt_s).collect();
        dts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = dts.len();
        let median_dt = if n % 2 == 1 {
            dts[n / 2]
        } else {
            (dts[n / 2 - 1] + dts[n / 2]) / 2.0
        };
        // Quartile via nearest-rank (no interpolation; fine for n ≤ 10).
        let q1 = dts[n / 4];
        let q3 = dts[(3 * n) / 4];
        let iqr = (q3 - q1).abs();
        Some(DtPrior {
            median_dt,
            iqr,
            sighting_count: n,
        })
    }

    /// Number of distinct callsigns currently tracked. (Test/diagnostic helper.)
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if no callsigns are currently tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// -- a7 recent-call table (hb-048) -----------------------------------------

/// A callsign decoded in a recent slot, with the metadata hb-048's a7
/// cross-correlation pass needs to enumerate next-slot templates.
///
/// hb-048 spec §"Mechanism" describes the WSJT-X `ft8_a7_save`
/// even/odd table this mirrors.
#[derive(Clone, Debug)]
pub struct A7ExpectedCall {
    /// Callsign decoded in the prior slot.
    pub callsign: String,
    /// Audio frequency the call was decoded at (Hz; offset from dial).
    pub freq_hz: f64,
    /// Slot parity (even/odd) of the slot the call was decoded in. The
    /// a7 cross-correlation runs in the *opposite* parity (the next slot).
    /// Stored as a `u8` (`0` = even, `1` = odd) to avoid coupling this crate
    /// to the `pancetta_core::slot::SlotParity` type — the consumer maps.
    pub slot_parity: u8,
    /// Wall-clock decode time; used for age-based eviction.
    pub decoded_at: SystemTime,
}

/// Recent-call table for hb-048 a7 cross-correlation.
///
/// Capped by `capacity` (LRU-by-`decoded_at`) and `max_age`.
#[derive(Debug)]
pub struct A7RecentCallTable {
    /// Keyed by callsign; if the same callsign re-decodes, the entry is
    /// updated in place rather than duplicated.
    entries: HashMap<String, A7ExpectedCall>,
    capacity: usize,
    max_age: Duration,
}

impl Default for A7RecentCallTable {
    fn default() -> Self {
        Self::new(DEFAULT_A7_CAPACITY, DEFAULT_A7_MAX_AGE)
    }
}

impl A7RecentCallTable {
    /// Create a new recent-call table.
    pub fn new(capacity: usize, max_age: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            capacity,
            max_age,
        }
    }

    /// Record a decoded callsign + freq + slot parity. Evicts expired
    /// entries and (if at the cap) the oldest entry by `decoded_at`.
    pub fn record(&mut self, call: A7ExpectedCall) {
        self.evict_expired(call.decoded_at);
        self.entries.insert(call.callsign.clone(), call);
        while self.entries.len() > self.capacity {
            // LRU evict: find oldest by decoded_at and drop it.
            if let Some((victim, _)) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.decoded_at)
                .map(|(k, v)| (k.clone(), v.decoded_at))
            {
                self.entries.remove(&victim);
            } else {
                break;
            }
        }
    }

    /// Evict entries older than `max_age` relative to `now`. Idempotent.
    pub fn evict_expired(&mut self, now: SystemTime) {
        let max_age = self.max_age;
        self.entries
            .retain(|_, e| match now.duration_since(e.decoded_at) {
                Ok(age) => age <= max_age,
                Err(_) => true, // clock skew — keep
            });
    }

    /// Iterate live entries whose `slot_parity` matches `parity`.
    pub fn entries_with_parity(&self, parity: u8) -> impl Iterator<Item = &A7ExpectedCall> {
        self.entries
            .values()
            .filter(move |e| e.slot_parity == parity)
    }

    /// Return the entry for `callsign` if any.
    pub fn get(&self, callsign: &str) -> Option<&A7ExpectedCall> {
        self.entries.get(callsign)
    }

    /// Number of entries currently tracked.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if no entries are tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// -- Within-QSO context (hb-173) -------------------------------------------

/// Inferred phase of a QSO exchange — drives the template space hb-173
/// injects for the next slot. See hb-173 spec §"Mechanism".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QsoPhase {
    /// Caller sent grid; expect responder's report next.
    GridSent,
    /// Report sent (incl R-report); expect ack (RRR / RR73) next.
    ReportSent,
    /// Ack sent (RRR); expect RR73 / 73 next.
    AckSent,
    /// 73 / RR73 sent; QSO complete (entry eligible for eviction).
    Complete,
    /// Unknown phase (free-text payload, irregular exchange).
    Unknown,
}

/// Unordered callsign-pair key + audio-frequency-bin for [`WithinQsoContext`].
///
/// Two decodes with the same (call_a, call_b) pair within `freq_bin ± 1`
/// (±50 Hz at the default 25 Hz bin) match the same entry, regardless of
/// which callsign is the sender.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct QsoKey {
    /// Sorted callsign pair — order ignored.
    pub pair: (String, String),
    /// Audio-frequency bin: `(freq_hz / FREQ_BIN_HZ).round() as i32`. With
    /// the default `FREQ_BIN_HZ = 25.0`, an entry in bin K matches any
    /// decode in bin K-1..=K+1 (±50 Hz total tolerance).
    pub freq_bin: i32,
}

/// Default frequency-bin width for `QsoKey::from_pair`.
pub const DEFAULT_QSO_FREQ_BIN_HZ: f64 = 25.0;

impl QsoKey {
    /// Build a key from two callsigns + a frequency, sorting the pair
    /// alphabetically so order-independence is structural.
    pub fn from_pair(a: &str, b: &str, freq_hz: f64) -> Self {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let freq_bin = (freq_hz / DEFAULT_QSO_FREQ_BIN_HZ).round() as i32;
        Self {
            pair: (lo.to_string(), hi.to_string()),
            freq_bin,
        }
    }
}

/// Per-pair QSO state — the value side of [`WithinQsoContext`].
#[derive(Clone, Debug)]
pub struct QsoState {
    /// Last decoded message text for this pair.
    pub last_message: String,
    /// Inferred phase classifier — drives which templates we inject next.
    pub phase: QsoPhase,
    /// Slot parity of the LAST responder (`0` = even, `1` = odd). The next
    /// expected turn is the OPPOSITE parity. `None` if the source decode
    /// didn't carry a parity tag.
    pub last_responder_parity: Option<u8>,
    /// Wall-clock last seen — used for TTL eviction.
    pub last_seen: SystemTime,
    /// Turn counter (1 = first directed exchange, 2 = first reply, …).
    pub turn_count: u32,
}

/// Within-QSO context: active-QSO state per callsign pair, used by hb-173
/// to inject pair-conditional templates into the residual decode pass.
///
/// Capped by `capacity` (LRU-by-`last_seen`) and `max_age` (default 90 s).
#[derive(Debug)]
pub struct WithinQsoContext {
    entries: HashMap<QsoKey, QsoState>,
    capacity: usize,
    max_age: Duration,
}

impl Default for WithinQsoContext {
    fn default() -> Self {
        Self::new(DEFAULT_WITHIN_QSO_CAPACITY, DEFAULT_WITHIN_QSO_MAX_AGE)
    }
}

impl WithinQsoContext {
    /// Create a new within-QSO context table.
    pub fn new(capacity: usize, max_age: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            capacity,
            max_age,
        }
    }

    /// Insert / update the entry for `key`. If the entry already exists,
    /// `turn_count` is incremented; otherwise it starts at 1.
    pub fn record(
        &mut self,
        key: QsoKey,
        last_message: String,
        phase: QsoPhase,
        last_responder_parity: Option<u8>,
        now: SystemTime,
    ) {
        self.evict_expired(now);
        let entry = self.entries.entry(key).or_insert_with(|| QsoState {
            last_message: last_message.clone(),
            phase,
            last_responder_parity,
            last_seen: now,
            turn_count: 0,
        });
        entry.last_message = last_message;
        entry.phase = phase;
        entry.last_responder_parity = last_responder_parity;
        entry.last_seen = now;
        entry.turn_count = entry.turn_count.saturating_add(1);

        while self.entries.len() > self.capacity {
            if let Some((victim, _)) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_seen)
                .map(|(k, v)| (k.clone(), v.last_seen))
            {
                self.entries.remove(&victim);
            } else {
                break;
            }
        }
    }

    /// Evict entries older than `max_age` relative to `now`. Idempotent.
    pub fn evict_expired(&mut self, now: SystemTime) {
        let max_age = self.max_age;
        self.entries
            .retain(|_, e| match now.duration_since(e.last_seen) {
                Ok(age) => age <= max_age,
                Err(_) => true, // clock skew — keep
            });
    }

    /// Look up the QSO state for a callsign pair at a frequency. Returns
    /// `None` if no entry matches within `freq_bin ± 1`.
    pub fn lookup(&self, a: &str, b: &str, freq_hz: f64) -> Option<&QsoState> {
        let base = QsoKey::from_pair(a, b, freq_hz);
        // Exact bin first.
        if let Some(s) = self.entries.get(&base) {
            return Some(s);
        }
        // ±1 bin tolerance.
        for delta in [-1i32, 1] {
            let mut key = base.clone();
            key.freq_bin = base.freq_bin.saturating_add(delta);
            if let Some(s) = self.entries.get(&key) {
                return Some(s);
            }
        }
        None
    }

    /// Number of active QSO entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if no entries are tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// -- Top-level container ----------------------------------------------------

/// A minimal abstraction of a `DecodedMessage` for the purpose of
/// updating the cross-time tables.
///
/// This avoids a `pancetta-qso → pancetta-ft8` dependency edge — the
/// coordinator translates from `pancetta_ft8::DecodedMessage` into this
/// shape before calling [`CrossTimeState::record_decode`].
#[derive(Clone, Debug)]
pub struct DecodeRecord {
    /// Sender callsign (the bare callsign, no `<...>` brackets or suffixes).
    /// `None` if the decoder couldn't extract a sender (free-text decodes,
    /// CQ-only with no callsign, etc.) — the cross-time tables are
    /// callsign-keyed, so such decodes are skipped.
    pub from_callsign: Option<String>,
    /// Recipient callsign, if the message was directed.
    /// `None` for CQ / free-text / status-only.
    pub to_callsign: Option<String>,
    /// Raw text of the decoded message (used for [`QsoState::last_message`]
    /// and phase classification — implemented in a future Session).
    pub text: String,
    /// Audio frequency offset (Hz from dial).
    pub frequency_hz: f64,
    /// Time offset (seconds, slot-relative) — the DT hb-057 records.
    pub time_offset_s: f64,
    /// Slot parity (`0` = even, `1` = odd). `None` if untagged.
    pub slot_parity: Option<u8>,
    /// Wall-clock decode time.
    pub at: SystemTime,
}

/// Coordinator-level cross-slot state, shared across decoder threads.
///
/// Construct with [`CrossTimeState::empty`] (default capacities) or
/// [`CrossTimeState::with_capacity`] (custom). Wrap in `Arc` and clone
/// the `Arc` into each consumer.
///
/// Each inner table is independently locked; concurrent reads of one
/// table never block writes to another.
///
/// # Usage sketch
///
/// ```rust,ignore
/// use std::sync::Arc;
/// use pancetta_qso::cross_time_state::{CrossTimeState, DecodeRecord};
///
/// // Coordinator startup:
/// let cross_time = Arc::new(CrossTimeState::empty());
///
/// // Decoder thread (per decoded message, post-FP-filter):
/// let rec = DecodeRecord { /* … */ ; unimplemented!() };
/// cross_time.record_decode(&rec);
///
/// // Future consumer (hb-057):
/// let dt_history = cross_time.dt_history.read().unwrap();
/// if let Some(prior) = dt_history.prior("K1ABC") {
///     // narrow sync search around prior.median_dt …
/// }
/// ```
pub struct CrossTimeState {
    /// hb-057 per-callsign DT history.
    pub dt_history: RwLock<CallsignDtHistory>,
    /// hb-048 per-slot a7 expected-call table.
    pub a7_recent_calls: RwLock<A7RecentCallTable>,
    /// hb-173 within-QSO context table.
    pub within_qso: RwLock<WithinQsoContext>,
}

impl CrossTimeState {
    /// Construct a `CrossTimeState` with default capacities & ages.
    pub fn empty() -> Self {
        Self {
            dt_history: RwLock::new(CallsignDtHistory::default()),
            a7_recent_calls: RwLock::new(A7RecentCallTable::default()),
            within_qso: RwLock::new(WithinQsoContext::default()),
        }
    }

    /// Construct with explicit per-table capacities. Pass `None` to use
    /// the default for that table.
    pub fn with_capacity(
        dt_history: Option<CallsignDtHistory>,
        a7_recent_calls: Option<A7RecentCallTable>,
        within_qso: Option<WithinQsoContext>,
    ) -> Self {
        Self {
            dt_history: RwLock::new(dt_history.unwrap_or_default()),
            a7_recent_calls: RwLock::new(a7_recent_calls.unwrap_or_default()),
            within_qso: RwLock::new(within_qso.unwrap_or_default()),
        }
    }

    /// Convenience wrapper: build a shared handle (`Arc<CrossTimeState>`).
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::empty())
    }

    /// Record a decoded message into all three tables.
    ///
    /// This is the single hook coordinator FT8 thread calls after a
    /// decode survives the FP filter. The semantics:
    ///
    /// - **DT history**: append a sighting if `from_callsign` is present.
    /// - **a7 recent-call table**: insert/update an entry if both
    ///   `from_callsign` and `slot_parity` are present.
    /// - **Within-QSO context**: insert/update an entry if both
    ///   `from_callsign` and `to_callsign` are present (i.e. the message
    ///   is directed). Phase is left [`QsoPhase::Unknown`] until the
    ///   hb-173 Session 2 classifier lands.
    ///
    /// Acquires write-locks on each inner table independently. Writes are
    /// best-effort: a poisoned lock results in a silent no-op for that
    /// table (the panic on the writer thread has already been logged
    /// upstream).
    pub fn record_decode(&self, rec: &DecodeRecord) {
        if let Some(ref call) = rec.from_callsign {
            if let Ok(mut dt) = self.dt_history.write() {
                dt.record(call, rec.time_offset_s, rec.at);
            }
            if let Some(parity) = rec.slot_parity {
                if let Ok(mut a7) = self.a7_recent_calls.write() {
                    a7.record(A7ExpectedCall {
                        callsign: call.clone(),
                        freq_hz: rec.frequency_hz,
                        slot_parity: parity,
                        decoded_at: rec.at,
                    });
                }
            }
            if let Some(ref to) = rec.to_callsign {
                if let Ok(mut wq) = self.within_qso.write() {
                    let key = QsoKey::from_pair(call, to, rec.frequency_hz);
                    wq.record(
                        key,
                        rec.text.clone(),
                        QsoPhase::Unknown,
                        rec.slot_parity,
                        rec.at,
                    );
                }
            }
        }
    }

    /// Evict expired entries from all three tables. Idempotent; safe to
    /// call from a periodic sweep task.
    pub fn evict_expired(&self, now: SystemTime) {
        if let Ok(mut dt) = self.dt_history.write() {
            dt.evict_expired(now);
        }
        if let Ok(mut a7) = self.a7_recent_calls.write() {
            a7.evict_expired(now);
        }
        if let Ok(mut wq) = self.within_qso.write() {
            wq.evict_expired(now);
        }
    }
}

impl Default for CrossTimeState {
    fn default() -> Self {
        Self::empty()
    }
}

// -- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    #[test]
    fn empty_state_queries_return_empty() {
        let cts = CrossTimeState::empty();
        assert_eq!(cts.dt_history.read().unwrap().len(), 0);
        assert!(cts.dt_history.read().unwrap().is_empty());
        assert_eq!(cts.a7_recent_calls.read().unwrap().len(), 0);
        assert!(cts.a7_recent_calls.read().unwrap().is_empty());
        assert_eq!(cts.within_qso.read().unwrap().len(), 0);
        assert!(cts.within_qso.read().unwrap().is_empty());

        assert!(cts.dt_history.read().unwrap().prior("K1ABC").is_none());
        assert!(cts.a7_recent_calls.read().unwrap().get("K1ABC").is_none());
        assert!(cts
            .within_qso
            .read()
            .unwrap()
            .lookup("K1ABC", "W1XYZ", 1500.0)
            .is_none());
    }

    #[test]
    fn dt_history_records_and_returns_median() {
        let mut h = CallsignDtHistory::default();
        let now = t0();
        h.record("K1ABC", 0.2, now);
        h.record("K1ABC", 0.4, now + Duration::from_secs(15));
        h.record("K1ABC", 0.3, now + Duration::from_secs(30));
        let prior = h
            .prior("K1ABC")
            .expect("prior should exist for 3 sightings");
        // Sorted [0.2, 0.3, 0.4] → median = 0.3
        assert!((prior.median_dt - 0.3).abs() < 1e-9);
        assert_eq!(prior.sighting_count, 3);
    }

    #[test]
    fn dt_history_min_sightings_gate() {
        let mut h = CallsignDtHistory::default();
        h.record("K1ABC", 0.5, t0());
        assert!(
            h.prior("K1ABC").is_none(),
            "single sighting must not yield prior"
        );
    }

    #[test]
    fn dt_history_capacity_drops_oldest() {
        let mut h = CallsignDtHistory::new(3, Duration::from_secs(60 * 60), 100);
        let base = t0();
        for i in 0..5 {
            h.record("K1ABC", i as f64 * 0.1, base + Duration::from_secs(i * 10));
        }
        let prior = h.prior("K1ABC").unwrap();
        // Only the last 3 (0.2, 0.3, 0.4) survive → median = 0.3
        assert_eq!(prior.sighting_count, 3);
        assert!((prior.median_dt - 0.3).abs() < 1e-9);
    }

    #[test]
    fn dt_history_evicts_expired() {
        let mut h = CallsignDtHistory::new(10, Duration::from_secs(60), 100);
        let base = t0();
        h.record("K1ABC", 0.2, base);
        h.record("K1ABC", 0.3, base + Duration::from_secs(5));
        // Advance well past the eviction window.
        let later = base + Duration::from_secs(200);
        h.evict_expired(later);
        assert!(h.is_empty(), "expired sightings should clear the callsign");
        assert!(h.prior("K1ABC").is_none());
    }

    #[test]
    fn a7_table_records_and_looks_up() {
        let mut t = A7RecentCallTable::default();
        let now = t0();
        t.record(A7ExpectedCall {
            callsign: "K1ABC".into(),
            freq_hz: 1500.0,
            slot_parity: 0,
            decoded_at: now,
        });
        let entry = t.get("K1ABC").expect("should be present");
        assert_eq!(entry.freq_hz, 1500.0);
        assert_eq!(entry.slot_parity, 0);
        // Parity filter.
        assert_eq!(t.entries_with_parity(0).count(), 1);
        assert_eq!(t.entries_with_parity(1).count(), 0);
    }

    #[test]
    fn a7_table_evicts_expired() {
        let mut t = A7RecentCallTable::new(32, Duration::from_secs(30));
        let now = t0();
        t.record(A7ExpectedCall {
            callsign: "K1ABC".into(),
            freq_hz: 1500.0,
            slot_parity: 0,
            decoded_at: now,
        });
        t.evict_expired(now + Duration::from_secs(60));
        assert!(t.is_empty());
    }

    #[test]
    fn within_qso_pair_key_is_order_independent() {
        let k1 = QsoKey::from_pair("K1ABC", "W1XYZ", 1500.0);
        let k2 = QsoKey::from_pair("W1XYZ", "K1ABC", 1500.0);
        assert_eq!(k1, k2);
    }

    #[test]
    fn within_qso_lookup_tolerates_neighbor_bin() {
        let mut w = WithinQsoContext::default();
        let now = t0();
        let key = QsoKey::from_pair("K1ABC", "W1XYZ", 1500.0);
        w.record(
            key,
            "K1ABC W1XYZ EM10".into(),
            QsoPhase::GridSent,
            Some(0),
            now,
        );
        // Same pair, slightly different freq (still within ±1 bin of 60 = bin 60).
        // 1500.0 / 25.0 = 60.0; 1525.0 / 25.0 = 61.0 — adjacent bin, must match.
        assert!(w.lookup("K1ABC", "W1XYZ", 1525.0).is_some());
        // 1600.0 / 25.0 = 64.0 — 4 bins away, must not match.
        assert!(w.lookup("K1ABC", "W1XYZ", 1600.0).is_none());
    }

    #[test]
    fn within_qso_turn_count_increments() {
        let mut w = WithinQsoContext::default();
        let now = t0();
        let key = QsoKey::from_pair("K1ABC", "W1XYZ", 1500.0);
        w.record(
            key.clone(),
            "K1ABC W1XYZ EM10".into(),
            QsoPhase::GridSent,
            Some(0),
            now,
        );
        w.record(
            key.clone(),
            "W1XYZ K1ABC -12".into(),
            QsoPhase::ReportSent,
            Some(1),
            now + Duration::from_secs(15),
        );
        let state = w.lookup("K1ABC", "W1XYZ", 1500.0).unwrap();
        assert_eq!(state.turn_count, 2);
        assert_eq!(state.phase, QsoPhase::ReportSent);
    }

    #[test]
    fn within_qso_evicts_expired() {
        let mut w = WithinQsoContext::new(64, Duration::from_secs(90));
        let now = t0();
        let key = QsoKey::from_pair("K1ABC", "W1XYZ", 1500.0);
        w.record(
            key,
            "K1ABC W1XYZ EM10".into(),
            QsoPhase::GridSent,
            Some(0),
            now,
        );
        w.evict_expired(now + Duration::from_secs(200));
        assert!(w.is_empty());
    }

    #[test]
    fn record_decode_updates_all_three_tables() {
        let cts = CrossTimeState::empty();
        let rec = DecodeRecord {
            from_callsign: Some("K1ABC".into()),
            to_callsign: Some("W1XYZ".into()),
            text: "K1ABC W1XYZ EM10".into(),
            frequency_hz: 1500.0,
            time_offset_s: 0.2,
            slot_parity: Some(0),
            at: t0(),
        };
        cts.record_decode(&rec);
        cts.record_decode(&rec); // second sighting for DT prior
        assert_eq!(cts.dt_history.read().unwrap().len(), 1);
        assert!(cts.dt_history.read().unwrap().prior("K1ABC").is_some());
        assert_eq!(cts.a7_recent_calls.read().unwrap().len(), 1);
        assert_eq!(cts.within_qso.read().unwrap().len(), 1);
    }

    #[test]
    fn record_decode_skips_when_callsign_absent() {
        let cts = CrossTimeState::empty();
        let rec = DecodeRecord {
            from_callsign: None,
            to_callsign: None,
            text: "??? noise ???".into(),
            frequency_hz: 1500.0,
            time_offset_s: 0.0,
            slot_parity: Some(0),
            at: t0(),
        };
        cts.record_decode(&rec);
        assert!(cts.dt_history.read().unwrap().is_empty());
        assert!(cts.a7_recent_calls.read().unwrap().is_empty());
        assert!(cts.within_qso.read().unwrap().is_empty());
    }

    #[test]
    fn record_decode_cq_updates_dt_and_a7_but_not_within_qso() {
        let cts = CrossTimeState::empty();
        let rec = DecodeRecord {
            from_callsign: Some("K1ABC".into()),
            to_callsign: None, // CQ — no recipient
            text: "CQ K1ABC FN42".into(),
            frequency_hz: 1500.0,
            time_offset_s: 0.1,
            slot_parity: Some(0),
            at: t0(),
        };
        cts.record_decode(&rec);
        assert_eq!(cts.dt_history.read().unwrap().len(), 1);
        assert_eq!(cts.a7_recent_calls.read().unwrap().len(), 1);
        assert!(cts.within_qso.read().unwrap().is_empty());
    }

    #[test]
    fn evict_expired_clears_all_three_tables() {
        let cts = CrossTimeState::empty();
        let rec = DecodeRecord {
            from_callsign: Some("K1ABC".into()),
            to_callsign: Some("W1XYZ".into()),
            text: "K1ABC W1XYZ EM10".into(),
            frequency_hz: 1500.0,
            time_offset_s: 0.2,
            slot_parity: Some(0),
            at: t0(),
        };
        cts.record_decode(&rec);
        // Force expiry on the strictest age (a7's 30 s).
        cts.evict_expired(t0() + Duration::from_secs(60 * 60 + 1));
        assert!(cts.dt_history.read().unwrap().is_empty());
        assert!(cts.a7_recent_calls.read().unwrap().is_empty());
        assert!(cts.within_qso.read().unwrap().is_empty());
    }

    #[test]
    fn concurrent_reads_and_writes_do_not_deadlock() {
        let cts = Arc::new(CrossTimeState::empty());
        let writer = {
            let cts = cts.clone();
            thread::spawn(move || {
                for i in 0..200 {
                    let call = format!("K{}AAA", i % 5);
                    cts.record_decode(&DecodeRecord {
                        from_callsign: Some(call),
                        to_callsign: Some("W1XYZ".into()),
                        text: "K1ABC W1XYZ EM10".into(),
                        frequency_hz: 1500.0 + (i as f64) * 0.1,
                        time_offset_s: 0.2,
                        slot_parity: Some((i % 2) as u8),
                        at: t0() + Duration::from_millis(i as u64),
                    });
                }
            })
        };
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let cts = cts.clone();
                thread::spawn(move || {
                    for _ in 0..200 {
                        let _ = cts.dt_history.read().unwrap().prior("K0AAA");
                        let _ = cts.a7_recent_calls.read().unwrap().get("K0AAA");
                        let _ = cts
                            .within_qso
                            .read()
                            .unwrap()
                            .lookup("K0AAA", "W1XYZ", 1500.0);
                    }
                })
            })
            .collect();
        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }
        // Final consistency: writes landed.
        assert!(!cts.dt_history.read().unwrap().is_empty());
        assert!(!cts.a7_recent_calls.read().unwrap().is_empty());
        assert!(!cts.within_qso.read().unwrap().is_empty());
    }

    #[test]
    fn shared_returns_arc_wrapping_default() {
        let s = CrossTimeState::shared();
        assert!(s.dt_history.read().unwrap().is_empty());
        // Two clones share the same lock.
        let s2 = s.clone();
        s.record_decode(&DecodeRecord {
            from_callsign: Some("K1ABC".into()),
            to_callsign: None,
            text: "CQ K1ABC".into(),
            frequency_hz: 1500.0,
            time_offset_s: 0.0,
            slot_parity: Some(0),
            at: t0(),
        });
        assert_eq!(s2.dt_history.read().unwrap().len(), 1);
    }
}
