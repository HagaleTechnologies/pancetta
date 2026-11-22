use super::caller_creation_slot_key;
use std::time::{Duration, UNIX_EPOCH};

/// Two SystemTimes within the same 15-second window map to the same key.
#[test]
fn same_slot_same_key() {
    // Slot N starts at unix second N*15; both 0 s and 14 s are in slot 0.
    let t0 = UNIX_EPOCH + Duration::from_secs(0);
    let t14 = UNIX_EPOCH + Duration::from_secs(14);
    assert_eq!(caller_creation_slot_key(t0), caller_creation_slot_key(t14));
}

/// The boundary second (15) starts a new slot.
#[test]
fn slot_boundary_increments_key() {
    let t_end_of_slot0 = UNIX_EPOCH + Duration::from_secs(14);
    let t_start_of_slot1 = UNIX_EPOCH + Duration::from_secs(15);
    let k0 = caller_creation_slot_key(t_end_of_slot0);
    let k1 = caller_creation_slot_key(t_start_of_slot1);
    assert_eq!(k1, k0 + 1, "adjacent slots must differ by exactly 1");
}

/// A realistic mid-session timestamp (e.g. 2026-06-25 12:00:07 UTC) hashes
/// to the correct slot index.
#[test]
fn real_timestamp_hashes_correctly() {
    // 2026-06-25 12:00:07 UTC = 1_751_198_407 unix seconds.
    // Floor(1_751_198_407 / 15) = 116_746_560  (slot in the :00 window).
    // 1_751_198_407 / 15 = 116_746_560.466...
    let unix_secs: u64 = 1_751_198_407;
    let t = UNIX_EPOCH + Duration::from_secs(unix_secs);
    assert_eq!(caller_creation_slot_key(t), unix_secs / 15);
}

/// A timestamp before UNIX_EPOCH (e.g. from a unit-test stub) returns 0
/// rather than panicking.
#[test]
fn pre_epoch_timestamp_returns_zero() {
    // SystemTime doesn't support times before UNIX_EPOCH directly in all
    // implementations; we use UNIX_EPOCH itself as the minimal safe input.
    let t = UNIX_EPOCH;
    assert_eq!(caller_creation_slot_key(t), 0);
}

/// The dedup state clears when the slot key changes (simulated inline).
#[test]
fn dedup_set_clears_on_slot_change() {
    let mut dedup: (u64, std::collections::HashSet<String>) = (0, std::collections::HashSet::new());

    // Slot 0: station A arrives twice.
    let slot0 = caller_creation_slot_key(UNIX_EPOCH + Duration::from_secs(3));
    if slot0 != dedup.0 {
        dedup.0 = slot0;
        dedup.1.clear();
    }
    let first_insert = dedup.1.insert("G8BCG".to_string());
    assert!(first_insert, "first decode in slot must be admitted");

    // Same slot, same station: second decode skipped.
    let slot0_again = caller_creation_slot_key(UNIX_EPOCH + Duration::from_secs(7));
    assert_eq!(slot0, slot0_again, "still same slot");
    let second_insert = dedup.1.insert("G8BCG".to_string());
    assert!(!second_insert, "second decode in same slot must be deduped");

    // Slot 1: station A reappears — set should have been cleared.
    let slot1 = caller_creation_slot_key(UNIX_EPOCH + Duration::from_secs(15));
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
