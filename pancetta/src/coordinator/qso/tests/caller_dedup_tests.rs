use super::caller_creation_slot_key;
use jiff::SignedDuration;
use pancetta_core::slot::SlotParity;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// FT8 period and DSP decode phase (`derive_dsp_timing`: 15 s - 2 s margin).
const FT8_NS: i64 = 15_000_000_000;
const FT8_PHASE: SignedDuration = SignedDuration::from_secs(13);
/// FT4 period and decode phase (7.5 s - 1 s margin).
const FT4_NS: i64 = 7_500_000_000;
const FT4_PHASE: SignedDuration = SignedDuration::from_millis(6_500);
/// FT2 period and decode phase (3.2 s - 1 s margin).
const FT2_NS: i64 = 3_200_000_000;
const FT2_PHASE: SignedDuration = SignedDuration::from_millis(2_200);

/// 2026-06-25 12:00:00 UTC, a minute boundary: an Even FT8 and FT4 slot start.
const BASE_SECS: u64 = 1_782_388_800;

fn at_ns(ns: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(ns)
}

fn parity_of_index(index: i64) -> SlotParity {
    if index.rem_euclid(2) == 0 {
        SlotParity::Even
    } else {
        SlotParity::Odd
    }
}

/// FT8 key with no tracked parity, the shape of the pre-PAN-477 tests.
fn ft8_key(t: SystemTime) -> i64 {
    caller_creation_slot_key(t, None, FT8_PHASE, FT8_NS)
}

/// Two decodes from the same FT8 window (stamped after the 13 s cut) map to
/// the same key.
#[test]
fn same_slot_same_key() {
    // Slot 0's window is cut at 13 s; its decodes are stamped 13 s .. 15+ s.
    let t13 = UNIX_EPOCH + Duration::from_secs(13);
    let t27 = UNIX_EPOCH + Duration::from_secs(27);
    assert_eq!(ft8_key(t13), ft8_key(t27));
}

/// The next window's decode point (28 s = slot 1 + 13 s) starts a new key.
#[test]
fn slot_boundary_increments_key() {
    let t_end_of_slot0 = UNIX_EPOCH + Duration::from_secs(27);
    let t_start_of_slot1 = UNIX_EPOCH + Duration::from_secs(28);
    let k0 = ft8_key(t_end_of_slot0);
    let k1 = ft8_key(t_start_of_slot1);
    assert_eq!(k1, k0 + 1, "adjacent slots must differ by exactly 1");
}

/// A realistic mid-session decode (2026-06-25 12:00:14 UTC, stamped 14 s
/// into the :00 window) hashes to that window's FT8 slot index.
#[test]
fn real_timestamp_hashes_correctly() {
    let unix_secs: u64 = BASE_SECS + 14;
    let t = UNIX_EPOCH + Duration::from_secs(unix_secs);
    let expected = (BASE_SECS / 15) as i64;
    assert_eq!(ft8_key(t), expected);
    assert_eq!(
        caller_creation_slot_key(t, Some(SlotParity::Even), FT8_PHASE, FT8_NS),
        expected
    );
}

/// UNIX_EPOCH itself (the earliest `SystemTime` a unit-test stub produces)
/// maps to a key without panicking: it sits before slot 0's decode point,
/// so it belongs to slot -1.
#[test]
fn epoch_timestamp_maps_to_a_key_without_panicking() {
    assert_eq!(ft8_key(UNIX_EPOCH), -1);
    assert_eq!(ft8_key(UNIX_EPOCH + Duration::from_secs(13)), 0);
    // A non-positive period falls back to FT8's, never divides by zero.
    assert_eq!(
        caller_creation_slot_key(UNIX_EPOCH + Duration::from_secs(13), None, FT8_PHASE, 0),
        0
    );
}

/// The dedup state clears when the slot key changes (simulated inline).
#[test]
fn dedup_set_clears_on_slot_change() {
    let mut dedup: (i64, std::collections::HashSet<String>) =
        (i64::MIN, std::collections::HashSet::new());

    // Slot 0: station A arrives twice.
    let slot0 = ft8_key(UNIX_EPOCH + Duration::from_secs(13));
    if slot0 != dedup.0 {
        dedup.0 = slot0;
        dedup.1.clear();
    }
    let first_insert = dedup.1.insert("G8BCG".to_string());
    assert!(first_insert, "first decode in slot must be admitted");

    // Same slot, same station: second decode skipped.
    let slot0_again = ft8_key(UNIX_EPOCH + Duration::from_millis(14_500));
    assert_eq!(slot0, slot0_again, "still same slot");
    let second_insert = dedup.1.insert("G8BCG".to_string());
    assert!(!second_insert, "second decode in same slot must be deduped");

    // Slot 1: station A reappears — set should have been cleared.
    let slot1 = ft8_key(UNIX_EPOCH + Duration::from_secs(28));
    assert_ne!(slot0, slot1);
    if slot1 != dedup.0 {
        dedup.0 = slot1;
        dedup.1.clear();
    }
    let third_insert = dedup.1.insert("G8BCG".to_string());
    assert!(
        third_insert,
        "first decode in new slot must be admitted again"
    );
}

/// PAN-477: in FT4 and FT2, decodes from consecutive slots (each stamped
/// 300 ms after that slot's decode point, carrying that slot's parity) get
/// distinct, consecutive keys. The old `/ 15` key put up to two FT4 slots
/// and four or five FT2 slots on one key.
#[test]
fn ft4_and_ft2_consecutive_slots_get_distinct_keys() {
    for (name, slot_ns, phase) in [("FT4", FT4_NS, FT4_PHASE), ("FT2", FT2_NS, FT2_PHASE)] {
        let base_ns = BASE_SECS * 1_000_000_000;
        let first_index = (base_ns / slot_ns as u64) as i64;
        let phase_ns = phase.as_nanos() as u64;
        let mut previous: Option<i64> = None;
        for step in 0..8u64 {
            let index = first_index + step as i64;
            let slot_start = index as u64 * slot_ns as u64;
            let stamp = at_ns(slot_start + phase_ns + 300_000_000);
            let key = caller_creation_slot_key(stamp, Some(parity_of_index(index)), phase, slot_ns);
            assert_eq!(key, index, "{name}: slot {step} keys on its own index");
            if let Some(previous) = previous {
                assert_eq!(
                    key,
                    previous + 1,
                    "{name}: consecutive slots, distinct keys"
                );
            }
            previous = Some(key);
        }
    }
}

/// PAN-477: one FT8 window whose decodes are stamped 14.9 s and 15.1 s after
/// its (Even) slot start keeps one key across the 15 s mark.
#[test]
fn one_ft8_window_decoded_across_the_mark_keeps_one_key() {
    let slot_start_ns = BASE_SECS * 1_000_000_000;
    let before_mark = at_ns(slot_start_ns + 14_900_000_000);
    let after_mark = at_ns(slot_start_ns + 15_100_000_000);
    let k_before = caller_creation_slot_key(before_mark, Some(SlotParity::Even), FT8_PHASE, FT8_NS);
    let k_after = caller_creation_slot_key(after_mark, Some(SlotParity::Even), FT8_PHASE, FT8_NS);
    assert_eq!(k_before, k_after, "one window, one key across the mark");
    assert_eq!(k_before, (BASE_SECS / 15) as i64);
}

/// PAN-477: a decode stamped one whole slot late (decode latency past the
/// next window's decode point) but carrying the earlier slot's parity keys
/// on that earlier slot, in every mode.
#[test]
fn key_aligns_to_the_decode_parity() {
    for (name, slot_ns, phase) in [
        ("FT8", FT8_NS, FT8_PHASE),
        ("FT4", FT4_NS, FT4_PHASE),
        ("FT2", FT2_NS, FT2_PHASE),
    ] {
        let base_ns = BASE_SECS * 1_000_000_000;
        let index = (base_ns / slot_ns as u64) as i64;
        let slot_start = index as u64 * slot_ns as u64;
        let late = at_ns(slot_start + slot_ns as u64 + phase.as_nanos() as u64 + 500_000_000);
        assert_eq!(
            caller_creation_slot_key(late, None, phase, slot_ns),
            index + 1,
            "{name}: unaligned, the late stamp falls in the next slot"
        );
        assert_eq!(
            caller_creation_slot_key(late, Some(parity_of_index(index)), phase, slot_ns),
            index,
            "{name}: the decode's parity pulls it back to the slot it was cut from"
        );
        // A matching parity leaves the index alone.
        assert_eq!(
            caller_creation_slot_key(late, Some(parity_of_index(index + 1)), phase, slot_ns),
            index + 1,
            "{name}: matching parity keeps the index"
        );
    }
}

/// PAN-477: with no tracked parity the key is the unaligned index
/// `floor((timestamp - decode_phase) / slot_ns)`.
#[test]
fn untracked_parity_uses_the_unaligned_index() {
    for (slot_ns, phase) in [
        (FT8_NS, FT8_PHASE),
        (FT4_NS, FT4_PHASE),
        (FT2_NS, FT2_PHASE),
    ] {
        for offset_ms in [0u64, 1_234, 7_499, 14_999, 30_001, 61_700] {
            let ns = BASE_SECS * 1_000_000_000 + offset_ms * 1_000_000;
            let expected = (ns as i64 - phase.as_nanos() as i64).div_euclid(slot_ns);
            assert_eq!(
                caller_creation_slot_key(at_ns(ns), None, phase, slot_ns),
                expected,
                "slot_ns {slot_ns}, offset {offset_ms} ms"
            );
        }
    }
}
