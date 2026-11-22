use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize};

/// PR #348 review (Codex P1): Fox mode admits up to `fox.max_streams`
/// Hound-answer QSOs plus its own independent CQ stream. Passing only
/// the operator's normal `autonomous.max_concurrent_qsos` (default 1)
/// into the coalescer/mid-TX-supersede cap would truncate Fox's intended
/// multi-stream bundle down to one signal, dropping the rest.
#[test]
fn fox_mode_raises_the_cap_above_the_normal_configured_value() {
    let fox_mode = AtomicBool::new(true);
    let fox_max_streams = AtomicUsize::new(5);
    assert_eq!(
        effective_max_concurrent_qsos(1, &fox_mode, &fox_max_streams),
        6,
        "Fox mode must raise the cap to fox_max_streams (Hounds) + 1 (its own CQ stream)"
    );
}

#[test]
fn non_fox_mode_is_unaffected() {
    let fox_mode = AtomicBool::new(false);
    let fox_max_streams = AtomicUsize::new(5);
    assert_eq!(
        effective_max_concurrent_qsos(2, &fox_mode, &fox_max_streams),
        2,
        "outside Fox mode the cap must be exactly the configured max_concurrent_qsos"
    );
}

#[test]
fn fox_mode_never_lowers_a_higher_configured_value() {
    let fox_mode = AtomicBool::new(true);
    let fox_max_streams = AtomicUsize::new(2);
    assert_eq!(
        effective_max_concurrent_qsos(10, &fox_mode, &fox_max_streams),
        10,
        "Fox mode raises the cap, it must never lower an already-higher configured value"
    );
}
