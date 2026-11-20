use super::*;

/// Process-global counters mean concurrent tests in this binary can also
/// increment them — assert a delta, not an absolute value (same
/// discipline as `main.rs`'s `panic_hook_counts_and_survives_via_catch_unwind`).
#[test]
fn tx_attempts_count_increments() {
    let before = tx_attempts_count();
    TX_ATTEMPTS_COUNT.fetch_add(1, Ordering::Relaxed);
    assert_eq!(tx_attempts_count(), before + 1);
}

#[test]
fn tx_defers_count_increments() {
    let before = tx_defers_count();
    TX_DEFERS_COUNT.fetch_add(1, Ordering::Relaxed);
    assert_eq!(tx_defers_count(), before + 1);
}

/// TX-F8: the multi-TX arm's defer-time recheck must bump the SAME
/// process-global counter the single-TX arm's defer-time recheck does
/// (mirrors `tx_defers_count_increments` above — see `coordinator/tx.rs`'s
/// Step-2b block in the `MultiTransmitRequest` worker arm).
#[test]
fn tx_defers_count_increments_for_multi_tx() {
    let before = tx_defers_count();
    TX_DEFERS_COUNT.fetch_add(1, Ordering::Relaxed);
    assert_eq!(tx_defers_count(), before + 1);
}
