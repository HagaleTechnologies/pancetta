//! hb-237 cross-sequence A7 callsign cache.
//!
//! # Purpose
//!
//! Maintain a cross-slot callsign cache so that callsigns decoded in slot N
//! become AP-type-7 (a7) candidates for the immediately following slot N+1.
//! This is the "cross-sequence A7" mechanism described in
//! `research/specs/spec-wsjtr-cross-sequence-a7.md` (inspired by spec ref;
//! original mechanism shipped in WSJT-X v2.6.0 `ft8_a7.f90`).
//!
//! FT8 QSOs alternate between two stations on opposite-parity 15-second slots.
//! Once station A is decoded in a slot of parity X, station A is likely to
//! reappear (as a target callsign in a reply) in the next slot of parity
//! ~X (i.e., the slot 15 s later). With both call1 and call2 constrained by
//! that prior, the effective FT8 message search collapses from ~2^77 to a
//! small enumerated set, buying ~6-8 dB of effective coding gain.
//!
//! # Scope of this module
//!
//! This module ships the **state container** for cross-sequence A7. It
//! intentionally does not implement the per-seed enumeration / fine-sync /
//! soft-symbol pipeline described in §8-§10 of the spec — that lives in
//! `pancetta-ft8` and is a follow-on. What ships here:
//!
//! - [`CrossSequenceCallCache`] — TTL- and capacity-bounded cache of recently
//!   decoded callsigns plus their freq/parity/timestamp.
//! - Trust-set gated [`CrossSequenceCallCache::record_decoded_trusted`] —
//!   the hb-237 spec calls out FP-amplification risk when seed callsigns
//!   are themselves false positives. The trust-gated record API consults
//!   the same hb-062 [`CallsignContinuityFilter`] used by the rest of the
//!   pipeline before admitting a callsign as a seed.
//!
//! Default behavior is **OFF** until corpus measurement (see
//! [`crate::callsign_continuity::CallsignContinuityFilter`] for the trust
//! set / FP filter the coordinator hooks in).
//!
//! # Threading
//!
//! The cache is owned by the coordinator as `Arc<RwLock<CrossSequenceCallCache>>`.
//! Writes happen on the FT8 decoder thread after a successful, FP-filter-
//! accepted decode; reads happen on the same thread at the start of the
//! next slot's decode pass. Cross-thread sharing follows the same pattern
//! as [`crate::cross_time_state::CrossTimeState`].
//!
//! # Slot semantics (PAN-477)
//!
//! - Every entry is stamped with the UTC start of the slot the DSP cut its
//!   decode window from (`DecodeWindow::slot` in the coordinator), and every
//!   query and eviction passes the current window's slot start and period.
//!   No wall clock is read here, so a clock step between windows cannot age
//!   an entry, and FT4/FT2 age on their own 7.5 s / 3.2 s grid.
//! - An entry's age is `(query_start - entry_start).div_euclid(slot_ns)`
//!   slots, on the query's period; it is admitted while that is at most
//!   `max_age_slots`.
//! - An entry whose slot start is after the query's is dropped. Window slot
//!   starts only increase, so only a backward clock resync produces one.
//! - Parity is stored as `0` = even, `1` = odd, of the entry's own slot.

use std::collections::HashMap;

use pancetta_core::time::UtcTimestamp;

use crate::callsign_continuity::CallsignContinuityFilter;

/// Default TTL for cache entries, measured in slots. Two slots (≈30 s)
/// covers one full QSO round-trip (TX → RX → TX → RX) at parity-alternation
/// rates without admitting stale seeds from old QSOs.
pub const DEFAULT_MAX_AGE_SLOTS: u32 = 2;

/// Default cache capacity. Bounded to prevent unbounded growth on busy
/// bands; LRU evicts the oldest entry once the cap is reached.
pub const DEFAULT_CAPACITY: usize = 64;

/// One entry in the cross-sequence callsign cache.
#[derive(Clone, Debug)]
pub struct A7SeedEntry {
    /// Callsign (uppercase, stripped suffix).
    pub callsign: String,
    /// Audio frequency offset (Hz from dial) at which the callsign was
    /// decoded. Used by the cross-sequence A7 pass to center its
    /// per-seed baseband extract on `prev_freq` (spec §5).
    pub freq_hz: f64,
    /// Slot parity of the slot the callsign was decoded in. `0` = even,
    /// `1` = odd. The opposite parity is the slot in which the callsign's
    /// QSO partner is *expected* to reply. Stored as a `u8` to avoid
    /// coupling to `pancetta_core::slot::SlotParity`.
    pub slot_parity: u8,
    /// UTC start of the slot the callsign was decoded in: the decode
    /// window's DSP slot (PAN-477). Used for TTL eviction.
    pub slot_start: UtcTimestamp,
}

/// A cached entry plus its private insertion sequence number, which orders
/// LRU eviction: entries recorded from one window share a slot start.
#[derive(Clone, Debug)]
struct CachedSeed {
    entry: A7SeedEntry,
    seq: u64,
}

/// Age of a slot starting at `entry_start` relative to the slot starting at
/// `query_start`, in whole slots of `slot_ns`; `None` when the entry's slot
/// starts after the query's.
fn age_slots(query_start: UtcTimestamp, entry_start: UtcTimestamp, slot_ns: i64) -> Option<i128> {
    let elapsed_ns = (query_start - entry_start).as_nanos();
    if elapsed_ns < 0 {
        return None;
    }
    Some(elapsed_ns.div_euclid(i128::from(slot_ns.max(1))))
}

/// Cross-sequence A7 callsign cache.
///
/// Stores recently decoded callsigns plus their freq / parity / slot start,
/// so that the next slot's decode can enumerate cross-sequence A7
/// candidates from them. Bounded by `capacity` (LRU by insertion order) and
/// `max_age_slots` (slots since `slot_start`, on the query's period).
///
/// Per-callsign keyed: if the same callsign is re-decoded later, the entry
/// is updated in place rather than duplicated.
#[derive(Debug)]
pub struct CrossSequenceCallCache {
    entries: HashMap<String, CachedSeed>,
    capacity: usize,
    max_age_slots: u32,
    next_seq: u64,
}

impl Default for CrossSequenceCallCache {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY, DEFAULT_MAX_AGE_SLOTS)
    }
}

impl CrossSequenceCallCache {
    /// Create a new cache with explicit capacity and max-age-in-slots.
    pub fn new(capacity: usize, max_age_slots: u32) -> Self {
        Self {
            entries: HashMap::new(),
            capacity: capacity.max(1),
            max_age_slots,
            next_seq: 0,
        }
    }

    /// Record a callsign decoded in the slot starting at `slot_start` on a
    /// `slot_ns` period. Performs lazy eviction of expired entries first,
    /// then inserts / updates the entry, then LRU-evicts to enforce
    /// capacity.
    ///
    /// Note: this is the **unfiltered** API. Production paths should use
    /// [`Self::record_decoded_trusted`] to gate insertion through the
    /// hb-062 trust set (mitigates the FP-amplification risk called out
    /// in the hb-237 spec §"FP risk").
    pub fn record_decoded(
        &mut self,
        callsign: &str,
        freq_hz: f64,
        slot_parity: u8,
        slot_start: UtcTimestamp,
        slot_ns: i64,
    ) {
        let upper = callsign.trim().to_uppercase();
        if upper.is_empty() {
            return;
        }
        self.evict_expired(slot_start, slot_ns);
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.entries.insert(
            upper.clone(),
            CachedSeed {
                entry: A7SeedEntry {
                    callsign: upper,
                    freq_hz,
                    slot_parity,
                    slot_start,
                },
                seq,
            },
        );
        self.enforce_capacity();
    }

    /// Trust-gated record: only insert the callsign if it passes the
    /// supplied `CallsignContinuityFilter`'s membership check. This is
    /// the mitigation called out in the hb-237 spec — if seed callsigns
    /// are FPs, A7 generates fake templates for the next slot. The
    /// trust filter already aggregates ADIF + cqdx + rolling-window
    /// callsign sources; consult it without expanding the rolling
    /// window (uses [`CallsignContinuityFilter::would_accept_callsign`]).
    ///
    /// Returns `true` if the callsign was admitted, `false` if filtered.
    pub fn record_decoded_trusted(
        &mut self,
        callsign: &str,
        freq_hz: f64,
        slot_parity: u8,
        slot_start: UtcTimestamp,
        slot_ns: i64,
        trust: &CallsignContinuityFilter,
    ) -> bool {
        let upper = callsign.trim().to_uppercase();
        if upper.is_empty() {
            return false;
        }
        if !trust.would_accept_callsign(&upper) {
            return false;
        }
        self.record_decoded(&upper, freq_hz, slot_parity, slot_start, slot_ns);
        true
    }

    /// Return all A7 candidate seeds whose age, in slots of `slot_ns` from
    /// their slot start to `now_slot_start` (the current window's slot), is
    /// at most `max_age_slots`. Caller-supplied `max_age_slots` lets
    /// callers tighten the window below the cache's configured ceiling
    /// (the cache may hold entries up to its own `max_age_slots`; the
    /// caller may want only the last N). An entry from a later slot than
    /// `now_slot_start` is not returned.
    ///
    /// Entries are not mutated by this call. Use [`Self::evict_expired`]
    /// to reclaim memory.
    pub fn get_a7_candidates(
        &self,
        now_slot_start: UtcTimestamp,
        slot_ns: i64,
        max_age_slots: u32,
    ) -> Vec<A7SeedEntry> {
        self.entries
            .values()
            .filter(|cached| {
                age_slots(now_slot_start, cached.entry.slot_start, slot_ns)
                    .is_some_and(|age| age <= i128::from(max_age_slots))
            })
            .map(|cached| cached.entry.clone())
            .collect()
    }

    /// Same as [`Self::get_a7_candidates`] but restricted to entries whose
    /// `slot_parity` matches `parity`. This is the production call: the
    /// next slot's A7 pass only consults the opposite-parity seeds.
    pub fn get_a7_candidates_with_parity(
        &self,
        now_slot_start: UtcTimestamp,
        slot_ns: i64,
        max_age_slots: u32,
        parity: u8,
    ) -> Vec<A7SeedEntry> {
        let mut out = self.get_a7_candidates(now_slot_start, slot_ns, max_age_slots);
        out.retain(|e| e.slot_parity == parity);
        out
    }

    /// Evict entries older than the cache's `max_age_slots` relative to the
    /// slot starting at `now_slot_start` on a `slot_ns` period, and entries
    /// from a later slot (a backward clock resync). Idempotent; safe to
    /// call from a periodic sweep.
    pub fn evict_expired(&mut self, now_slot_start: UtcTimestamp, slot_ns: i64) {
        let max_age = i128::from(self.max_age_slots);
        self.entries.retain(|_, cached| {
            age_slots(now_slot_start, cached.entry.slot_start, slot_ns)
                .is_some_and(|age| age <= max_age)
        });
    }

    /// Drop the oldest entries until the cap is satisfied. LRU by
    /// insertion order.
    fn enforce_capacity(&mut self) {
        while self.entries.len() > self.capacity {
            if let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, cached)| cached.seq)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&victim);
            } else {
                break;
            }
        }
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if no entries are tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Configured capacity ceiling.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Configured TTL in slots.
    pub fn max_age_slots(&self) -> u32 {
        self.max_age_slots
    }

    /// Membership check (test/diagnostic helper).
    pub fn contains(&self, callsign: &str) -> bool {
        self.entries.contains_key(&callsign.to_uppercase())
    }
}

#[cfg(test)]
mod cross_sequence_tests {
    use super::*;
    use jiff::SignedDuration;
    use std::collections::HashSet;

    /// FT8's slot period: the inline tests run on the 15 s grid.
    const SLOT: i64 = 15_000_000_000;

    /// 2026-01-01 00:00:00 UTC plus `offset_secs`; multiples of 15 are FT8
    /// slot starts.
    fn t(offset_secs: i64) -> UtcTimestamp {
        UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 0, 0).unwrap()
            + SignedDuration::from_secs(offset_secs)
    }

    #[test]
    fn cache_stores_and_retrieves_within_ttl() {
        let mut cache = CrossSequenceCallCache::new(16, 2);
        let slot_a = t(0);
        cache.record_decoded("K1ABC", 1200.0, 0, slot_a, SLOT);
        assert_eq!(cache.len(), 1);
        assert!(cache.contains("K1ABC"));

        // Within TTL (1 slot later = 15s).
        let slot_b = t(15);
        let seeds = cache.get_a7_candidates(slot_b, SLOT, 2);
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].callsign, "K1ABC");
        assert_eq!(seeds[0].slot_parity, 0);
        assert!((seeds[0].freq_hz - 1200.0).abs() < 1e-9);
    }

    #[test]
    fn ttl_eviction_drops_stale_entries() {
        let mut cache = CrossSequenceCallCache::new(16, 2);
        let slot_zero = t(0);
        cache.record_decoded("K1ABC", 1200.0, 0, slot_zero, SLOT);

        // 3 slots later = 45 s. With max_age_slots = 2, this is stale.
        let later = t(45);
        // Trigger eviction by recording another entry.
        cache.record_decoded("W2XYZ", 1500.0, 1, later, SLOT);
        assert_eq!(cache.len(), 1, "stale K1ABC should be evicted");
        assert!(cache.contains("W2XYZ"));
        assert!(!cache.contains("K1ABC"));
    }

    #[test]
    fn get_candidates_respects_caller_window() {
        let mut cache = CrossSequenceCallCache::new(16, 5);
        let now_ref = t(0);
        // Windows record in slot order (PAN-477: recording an older slot
        // drops entries from later slots, as after a backward resync).
        // 2 slots old at evaluation time.
        cache.record_decoded("W2XYZ", 1500.0, 1, t(-15), SLOT);
        // 1 slot old.
        cache.record_decoded("K1ABC", 1200.0, 0, t(0), SLOT);
        let now = now_ref + SignedDuration::from_secs(15);

        // Tight window: 1 slot — only the fresh entry survives.
        let tight = cache.get_a7_candidates(now, SLOT, 1);
        assert_eq!(tight.len(), 1);
        assert_eq!(tight[0].callsign, "K1ABC");

        // Looser window: 3 slots — both entries returned.
        let loose = cache.get_a7_candidates(now, SLOT, 3);
        assert_eq!(loose.len(), 2);
    }

    #[test]
    fn capacity_bound_enforced_lru() {
        let mut cache = CrossSequenceCallCache::new(2, 100);
        cache.record_decoded("K1AAA", 1000.0, 0, t(0), SLOT);
        cache.record_decoded("K1BBB", 1100.0, 1, t(15), SLOT);
        cache.record_decoded("K1CCC", 1200.0, 0, t(30), SLOT);
        assert_eq!(cache.len(), 2, "cap should evict to two");
        // K1AAA was inserted first → evicted.
        assert!(!cache.contains("K1AAA"));
        assert!(cache.contains("K1BBB"));
        assert!(cache.contains("K1CCC"));
    }

    #[test]
    fn same_callsign_updates_in_place_not_duplicated() {
        let mut cache = CrossSequenceCallCache::new(8, 4);
        cache.record_decoded("K1ABC", 1200.0, 0, t(0), SLOT);
        cache.record_decoded("K1ABC", 1250.0, 0, t(15), SLOT);
        assert_eq!(cache.len(), 1);
        let seeds = cache.get_a7_candidates(t(15), SLOT, 2);
        assert_eq!(seeds.len(), 1);
        // Updated freq_hz reflects most recent decode.
        assert!((seeds[0].freq_hz - 1250.0).abs() < 1e-9);
    }

    #[test]
    fn parity_filter_returns_only_matching_parity() {
        let mut cache = CrossSequenceCallCache::new(8, 4);
        cache.record_decoded("EVEN1", 1000.0, 0, t(0), SLOT);
        cache.record_decoded("EVEN2", 1100.0, 0, t(0), SLOT);
        cache.record_decoded("ODD1", 1200.0, 1, t(0), SLOT);

        let odd = cache.get_a7_candidates_with_parity(t(15), SLOT, 2, 1);
        assert_eq!(odd.len(), 1);
        assert_eq!(odd[0].callsign, "ODD1");

        let even = cache.get_a7_candidates_with_parity(t(15), SLOT, 2, 0);
        assert_eq!(even.len(), 2);
        let names: HashSet<_> = even.iter().map(|e| e.callsign.as_str()).collect();
        assert!(names.contains("EVEN1"));
        assert!(names.contains("EVEN2"));
    }

    #[test]
    fn empty_callsign_ignored() {
        let mut cache = CrossSequenceCallCache::new(8, 4);
        cache.record_decoded("", 1000.0, 0, t(0), SLOT);
        cache.record_decoded("   ", 1000.0, 0, t(0), SLOT);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn callsign_normalized_uppercase_and_trimmed() {
        let mut cache = CrossSequenceCallCache::new(8, 4);
        cache.record_decoded("  k1abc  ", 1200.0, 0, t(0), SLOT);
        assert!(cache.contains("K1ABC"));
        assert!(cache.contains("k1abc")); // contains() normalizes too
    }

    #[test]
    fn slot_n_decode_appears_in_slot_n_plus_one_hint_set() {
        // Direct simulation of the wiring contract: slot N decode -> slot N+1
        // hint set. Slot N starts at t=0 (even parity); the entry carries
        // that slot's start. Slot N+1 starts at t=15 (odd parity).
        let mut cache = CrossSequenceCallCache::new(16, 2);
        cache.record_decoded("K1ABC", 1200.0, /* even */ 0, t(0), SLOT);
        // At start of next slot:
        let next_slot_start = t(15);
        let hints = cache.get_a7_candidates(next_slot_start, SLOT, 2);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].callsign, "K1ABC");
    }

    #[test]
    fn callsign_three_or_more_slots_old_excluded() {
        // Same simulation but slot N+3: by then the entry from slot N is
        // outside the default 2-slot TTL.
        let mut cache = CrossSequenceCallCache::new(16, DEFAULT_MAX_AGE_SLOTS);
        cache.record_decoded("K1ABC", 1200.0, 0, t(0), SLOT);

        // Slot N+3 starts at 45s.
        let three_slots_later = t(45);
        let hints = cache.get_a7_candidates(three_slots_later, SLOT, DEFAULT_MAX_AGE_SLOTS);
        assert!(hints.is_empty(), "stale entry should not be returned");
    }

    #[test]
    fn trust_gated_record_admits_known_callsign() {
        let mut filter = CallsignContinuityFilter::new(64);
        filter.extend_from_iter(["K1ABC".to_string()]);
        let mut cache = CrossSequenceCallCache::new(8, 4);
        let admitted = cache.record_decoded_trusted("K1ABC", 1200.0, 0, t(0), SLOT, &filter);
        assert!(admitted);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn trust_gated_record_rejects_unknown_callsign() {
        // Strict mode: no static, no cqdx, no rolling — unknown call rejected.
        let filter = CallsignContinuityFilter::new(64);
        let mut cache = CrossSequenceCallCache::new(8, 4);
        let admitted = cache.record_decoded_trusted("XX9XYZ", 1200.0, 0, t(0), SLOT, &filter);
        assert!(!admitted);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn trust_gated_record_passes_in_lenient_cold_start() {
        // Lenient: anything passes until reference_size >= threshold.
        let filter = CallsignContinuityFilter::new_lenient(64, 5);
        let mut cache = CrossSequenceCallCache::new(8, 4);
        let admitted = cache.record_decoded_trusted("XX9XYZ", 1200.0, 0, t(0), SLOT, &filter);
        assert!(admitted, "lenient mode should admit during cold-start");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn default_constants_sane() {
        let cache = CrossSequenceCallCache::default();
        assert_eq!(cache.capacity(), DEFAULT_CAPACITY);
        assert_eq!(cache.max_age_slots(), DEFAULT_MAX_AGE_SLOTS);
        assert!(cache.is_empty());
    }

    #[test]
    fn evict_expired_idempotent() {
        let mut cache = CrossSequenceCallCache::new(8, 2);
        cache.record_decoded("K1ABC", 1200.0, 0, t(0), SLOT);
        let stale_eval = t(45);
        cache.evict_expired(stale_eval, SLOT);
        assert!(cache.is_empty());
        // Second call is no-op.
        cache.evict_expired(stale_eval, SLOT);
        assert!(cache.is_empty());
    }
}

#[cfg(test)]
#[path = "cross_sequence/tests/slot_ttl_tests.rs"]
mod slot_ttl_tests;
