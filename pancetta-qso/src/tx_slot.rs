//! The TX scheduler's current-slot rule, in one place (PAN-477).
//!
//! The TX worker's `schedule_tx` (in the `pancetta` crate) decides whether a
//! request keys in the CURRENT slot or defers to the next slot of its parity.
//! The QSO manager's keep-call stall ledger (`keep_call_tx_slot`) must
//! predict that same slot, or one real TX opportunity is counted twice
//! (PAN-128). Both used to carry their own copy of the rule and of the
//! worker's coalesce constants; both now call [`tx_target_slot`] and
//! [`current_slot_acceptance_cap_ms`], which work on the active slot period
//! in nanoseconds (FT8 15e9, FT4 7.5e9, FT2 3.2e9).
//!
//! This lives in `pancetta-qso` because `pancetta` already depends on it
//! (precedent: [`crate::fox::fox_slot_offset`]) and `pancetta-core` is the
//! publishable vocabulary crate (PAN-384), which should not carry TX-worker
//! tuning constants.

use pancetta_core::slot::{
    current_slot_start_with_period, next_slot_with_parity_with_period, SlotParity,
};
use pancetta_core::time::SignedDurationExt as _;
use pancetta_core::time::UtcTimestamp;

/// FT8-baseline length, in ms, of the TX worker's base coalesce-collection
/// wait. The worker scales it to the active period
/// (`coalesce_collect_window_ms`); see that function for the tuning history.
pub const COALESCE_COLLECT_WINDOW_MS: u64 = 800;

/// The TX worker's fixed (unscaled) safety margin, in ms, reserved below the
/// late-start cap for encode/modulate and other per-message overhead after
/// the coalesce point (PAN-117).
pub const COALESCE_CAP_SAFETY_MARGIN_MS: u64 = 500;

/// The FT8 slot length in ns, the baseline every FT8-tuned millisecond
/// constant is scaled from.
const FT8_SLOT_NS_F64: f64 = 15e9;

/// Scale an FT8-tuned duration in ms to the slot period `slot_ns`:
/// `round(ms * slot_ns / 15e9)`. FT8 (`slot_ns` = 15e9) returns `ms`
/// unchanged; FT4 halves it; FT2 scales by 3.2/15.
///
/// This is the same f64 the worker's `cycle_duration / 15.0` scaling gives
/// for every protocol (pinned by `pancetta`'s `tx_slot_agreement_tests`).
pub fn scale_ms_to_slot(ms: u64, slot_ns: i64) -> u64 {
    ((ms as f64) * (slot_ns as f64 / FT8_SLOT_NS_F64)).round() as u64
}

/// The queue-time current-slot acceptance cap, in ms, for a single
/// `TransmitRequest` that waits through the worker's base coalesce window
/// (PAN-117): the period-scaled `tx_late_max_ms`, minus the period-scaled
/// base coalesce window, minus the fixed safety margin, saturating at 0.
///
/// FT8 at the 3000 ms default gives 1700 ms; FT4 600 ms; FT2 0 ms.
pub fn current_slot_acceptance_cap_ms(slot_ns: i64, tx_late_max_ms: u64) -> u64 {
    scale_ms_to_slot(tx_late_max_ms, slot_ns)
        .saturating_sub(scale_ms_to_slot(COALESCE_COLLECT_WINDOW_MS, slot_ns))
        .saturating_sub(COALESCE_CAP_SAFETY_MARGIN_MS)
}

/// The slot a TX request at `now` targets, and whether it was deferred.
///
/// The CURRENT slot (of period `slot_ns`) is the target exactly when its
/// parity is `parity` AND `now` is at most `accept_cap_ms` into it;
/// otherwise the target is the next slot of `parity` strictly after `now`
/// and `deferred` is `true`. Returns `(target_slot_start, deferred)`.
pub fn tx_target_slot(
    now: UtcTimestamp,
    parity: SlotParity,
    accept_cap_ms: u64,
    slot_ns: i64,
) -> (UtcTimestamp, bool) {
    let cur_start = current_slot_start_with_period(now, slot_ns);
    let cur_parity = SlotParity::of_with_period(cur_start, slot_ns);
    let ms_into_cur_slot = (now - cur_start).as_millis_i64().max(0) as u64;
    if cur_parity == parity && ms_into_cur_slot <= accept_cap_ms {
        (cur_start, false)
    } else {
        (
            next_slot_with_parity_with_period(now, parity, slot_ns),
            true,
        )
    }
}

#[cfg(test)]
#[path = "tx_slot/tests/tx_target_slot_tests.rs"]
mod tx_target_slot_tests;
