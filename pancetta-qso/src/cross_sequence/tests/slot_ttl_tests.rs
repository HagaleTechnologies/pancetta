//! PAN-477: the A7 seed age counts slots of the query's period between DSP
//! slot starts. No wall clock is read, so FT4 and FT2 age on their own grid
//! and a clock step between windows cannot age an entry.

use super::{CrossSequenceCallCache, DEFAULT_MAX_AGE_SLOTS};
use jiff::SignedDuration;
use pancetta_core::time::UtcTimestamp;

const FT8_NS: i64 = 15_000_000_000;
const FT4_NS: i64 = 7_500_000_000;
const FT2_NS: i64 = 3_200_000_000;

/// The UTC start of slot `index` of a `slot_ns` period, counting from
/// 2026-01-01 00:00:00 UTC (a boundary of all three periods, slot 0 Even).
fn slot_start(index: i64, slot_ns: i64) -> UtcTimestamp {
    UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 0, 0).unwrap()
        + SignedDuration::from_nanos(index * slot_ns)
}

fn callsigns(mut seeds: Vec<super::A7SeedEntry>) -> Vec<String> {
    seeds.sort_by(|a, b| a.callsign.cmp(&b.callsign));
    seeds.into_iter().map(|s| s.callsign).collect()
}

/// Seeds 1 to 4 slots old, alternating parity, queried for the opposite
/// parity one slot after the newest: every mode returns only the 1-slot-old
/// seed. Before PAN-477 FT4 also returned the 3-slot-old one, because the
/// TTL was 30 s of wall time.
#[test]
fn opposite_parity_query_returns_only_the_previous_slot_in_every_mode() {
    for slot_ns in [FT8_NS, FT4_NS, FT2_NS] {
        // Query slot 4 (Even); seeds in slots 0..=3, so 4..=1 slots old.
        let mut cache = CrossSequenceCallCache::new(64, DEFAULT_MAX_AGE_SLOTS);
        for (index, call) in ["K4AAA", "K3AAA", "K2AAA", "K1AAA"].iter().enumerate() {
            let index = index as i64;
            let parity = (index % 2) as u8;
            cache.record_decoded(call, 1000.0, parity, slot_start(index, slot_ns), slot_ns);
        }
        // The query is the Even slot 4: its opposite parity is Odd (1).
        let seeds = cache.get_a7_candidates_with_parity(
            slot_start(4, slot_ns),
            slot_ns,
            DEFAULT_MAX_AGE_SLOTS,
            1,
        );
        assert_eq!(
            callsigns(seeds),
            vec!["K1AAA".to_string()],
            "{slot_ns} ns period"
        );
    }
}

/// The age is counted in slots of the query's period: an FT4 entry two FT4
/// slots old (15 s) is admitted with `max_age_slots = 2`, and three FT4
/// slots old (22.5 s) is not, although 22.5 s is under FT8's two slots.
#[test]
fn ttl_counts_slots_of_the_query_period() {
    let mut cache = CrossSequenceCallCache::new(64, 8);
    cache.record_decoded("K1AAA", 1000.0, 0, slot_start(0, FT4_NS), FT4_NS);

    let at = |index| callsigns(cache.get_a7_candidates(slot_start(index, FT4_NS), FT4_NS, 2));
    assert_eq!(at(0), vec!["K1AAA".to_string()], "same slot: age 0");
    assert_eq!(at(1), vec!["K1AAA".to_string()], "age 1");
    assert_eq!(at(2), vec!["K1AAA".to_string()], "age 2");
    assert!(at(3).is_empty(), "age 3 is past max_age_slots = 2");

    // A query instant inside a slot rounds the age down to whole slots.
    let mid_slot_two = slot_start(2, FT4_NS) + SignedDuration::from_millis(7_000);
    assert_eq!(
        callsigns(cache.get_a7_candidates(mid_slot_two, FT4_NS, 2)),
        vec!["K1AAA".to_string()]
    );

    // Eviction uses the same rule with the cache's own ceiling.
    let mut small = CrossSequenceCallCache::new(64, 2);
    small.record_decoded("K1AAA", 1000.0, 0, slot_start(0, FT2_NS), FT2_NS);
    small.evict_expired(slot_start(2, FT2_NS), FT2_NS);
    assert!(small.contains("K1AAA"), "age 2 is kept");
    small.evict_expired(slot_start(3, FT2_NS), FT2_NS);
    assert!(!small.contains("K1AAA"), "age 3 is evicted");
}

/// After a backward clock resync the query's slot can start before an
/// entry's: that entry is neither returned nor kept.
#[test]
fn an_entry_from_a_later_slot_than_the_query_is_dropped() {
    let mut cache = CrossSequenceCallCache::new(64, DEFAULT_MAX_AGE_SLOTS);
    cache.record_decoded("K1AAA", 1000.0, 1, slot_start(5, FT8_NS), FT8_NS);

    let seeds = cache.get_a7_candidates(slot_start(4, FT8_NS), FT8_NS, DEFAULT_MAX_AGE_SLOTS);
    assert!(
        seeds.is_empty(),
        "an entry from a later slot is not returned"
    );
    assert!(
        cache.contains("K1AAA"),
        "the query does not mutate the cache"
    );

    // Recording in the earlier slot evicts the later-slot entry.
    cache.record_decoded("K2AAA", 1000.0, 0, slot_start(4, FT8_NS), FT8_NS);
    assert!(!cache.contains("K1AAA"), "the later-slot entry is not kept");
    assert!(cache.contains("K2AAA"));
}

/// Entries decoded in one window share a slot start, so LRU eviction
/// follows insertion order.
#[test]
fn lru_eviction_within_one_slot_follows_insertion_order() {
    let mut cache = CrossSequenceCallCache::new(2, DEFAULT_MAX_AGE_SLOTS);
    let slot = slot_start(0, FT8_NS);
    cache.record_decoded("K1AAA", 1000.0, 0, slot, FT8_NS);
    cache.record_decoded("K2AAA", 1100.0, 0, slot, FT8_NS);
    cache.record_decoded("K3AAA", 1200.0, 0, slot, FT8_NS);
    assert_eq!(cache.len(), 2);
    assert!(!cache.contains("K1AAA"), "first inserted is evicted first");
    assert!(cache.contains("K2AAA"));
    assert!(cache.contains("K3AAA"));

    // Re-recording K2AAA makes it the newest, so K3AAA goes next.
    cache.record_decoded("K2AAA", 1150.0, 0, slot, FT8_NS);
    cache.record_decoded("K4AAA", 1300.0, 0, slot, FT8_NS);
    assert!(!cache.contains("K3AAA"));
    assert!(cache.contains("K2AAA"));
    assert!(cache.contains("K4AAA"));
}
