//! FT8 slot timing utilities.
//!
//! FT8 uses 15-second time slots aligned to UTC: every transmission begins at
//! 0/15/30/45 seconds within a minute, with a 0.5s pre-roll silence before the
//! audio actually starts. Receivers expect signal energy at slot+500ms; arriving
//! later shows up as a positive `DT` in WSJT-X.
//!
//! Computing the next slot boundary correctly requires sub-second precision —
//! `chrono::DateTime::timestamp() % 15` drops the fractional component and
//! averages ~500ms late depending on when it's called. Use the helpers here.

use chrono::{DateTime, Duration, Utc};

/// FT8 slot length.
pub const SLOT_NS: i64 = 15 * 1_000_000_000;

/// FT8 leading silence between slot boundary and start of symbol audio.
pub const PRE_ROLL_NS: i64 = 500_000_000;

/// FT8 transmission duration (79 symbols × 0.16s).
pub const TX_DURATION_NS: i64 = 12_640_000_000;

/// Returns the start of the slot that contains `t`. Used by the TX
/// scheduler to detect "we're inside a viable slot, target it" vs.
/// "current slot is wrong parity or already too late, advance."
pub fn current_slot_start(t: DateTime<Utc>) -> DateTime<Utc> {
    let ns = t
        .timestamp_nanos_opt()
        .expect("system clock out of i64 ns range");
    let slot_ns = ns.div_euclid(SLOT_NS) * SLOT_NS;
    DateTime::<Utc>::from_timestamp_nanos(slot_ns)
}

/// Parity of an FT8 slot. Even slots start at UTC seconds `:00` and
/// `:30`; Odd at `:15` and `:45`. Two stations in QSO must transmit on
/// opposite parities — same parity collides on air.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SlotParity {
    /// Slot index (timestamp_ns / SLOT_NS) is even — boundaries at :00 and :30.
    Even,
    /// Slot index is odd — boundaries at :15 and :45.
    Odd,
}

impl SlotParity {
    /// Parity of the slot containing `t`. Computed as `(t.timestamp / 15) % 2`,
    /// where the slot index is the floor — so any instant inside the slot
    /// resolves to the same parity as the slot's start boundary.
    pub fn of(t: DateTime<Utc>) -> SlotParity {
        let ns = t
            .timestamp_nanos_opt()
            .expect("system clock out of i64 ns range");
        let slot_index = ns.div_euclid(SLOT_NS);
        if slot_index % 2 == 0 {
            SlotParity::Even
        } else {
            SlotParity::Odd
        }
    }

    /// The other parity. `Even <-> Odd`. Idempotent under double-flip.
    pub fn opposite(self) -> SlotParity {
        match self {
            SlotParity::Even => SlotParity::Odd,
            SlotParity::Odd => SlotParity::Even,
        }
    }
}

/// Returns the next 15-second slot boundary at or after `now + min_lead`.
///
/// Use `min_lead = Duration::zero()` to allow returning `now` itself when it
/// happens to fall exactly on a boundary; pass a non-zero lead when the caller
/// needs guaranteed headroom (e.g., to engage PTT before the slot starts).
pub fn next_slot_start(now: DateTime<Utc>, min_lead: Duration) -> DateTime<Utc> {
    let now_ns = now
        .timestamp_nanos_opt()
        .expect("system clock out of i64 ns range");
    let lead_ns = min_lead.num_nanoseconds().unwrap_or(0).max(0);
    // If now is past the start of the current slot, ((now / SLOT) + 1) * SLOT
    // is the next boundary. If now is exactly on a boundary, this still picks
    // the next one — desirable when callers need lead time.
    let mut target_ns = ((now_ns / SLOT_NS) + 1) * SLOT_NS;
    while target_ns - now_ns < lead_ns {
        target_ns += SLOT_NS;
    }
    DateTime::<Utc>::from_timestamp_nanos(target_ns)
}

/// Returns the next slot start whose parity equals `wanted`, strictly after `now`.
///
/// Always advances past the current slot (i.e., `now` falling on the start of a
/// matching slot still returns the *next* matching slot). This matches the
/// semantics of `next_slot_start`: callers want a future TX target, never the
/// present one.
pub fn next_slot_with_parity(now: DateTime<Utc>, wanted: SlotParity) -> DateTime<Utc> {
    let mut candidate = next_slot_start(now, Duration::zero());
    if SlotParity::of(candidate) != wanted {
        candidate += Duration::nanoseconds(SLOT_NS);
    }
    candidate
}

/// Returns the next moment FT8 audio should begin (`next_slot_start + 500ms`).
///
/// `min_lead` is measured from `now` to the audio-start instant, NOT to the
/// slot boundary itself.
pub fn next_audio_start(now: DateTime<Utc>, min_lead: Duration) -> DateTime<Utc> {
    let pre_roll = Duration::nanoseconds(PRE_ROLL_NS);
    // The audio-start instant is slot_start + pre_roll. If the caller wants
    // the audio-start to be at least `min_lead` away, the slot boundary needs
    // to be at least (min_lead - pre_roll) away.
    let slot_lead = if min_lead > pre_roll {
        min_lead - pre_roll
    } else {
        Duration::zero()
    };
    next_slot_start(now, slot_lead) + pre_roll
}

/// Returns the next moment that is `offset` past a slot boundary.
///
/// Used for scheduling at a fixed phase within the 15s slot (e.g., the DSP
/// pipeline runs decoding at slot+13s, after the 12.64s transmission ends).
/// If the current slot's `start + offset` is still in the future, returns it;
/// otherwise advances to the next slot.
///
/// Panics if `offset >= 15s` (a phase past the end of the slot is meaningless).
pub fn next_phase(now: DateTime<Utc>, offset: Duration) -> DateTime<Utc> {
    let offset_ns = offset
        .num_nanoseconds()
        .expect("offset out of i64 ns range");
    assert!(
        offset_ns >= 0 && offset_ns < SLOT_NS,
        "phase offset must be in [0, 15s)"
    );
    let now_ns = now
        .timestamp_nanos_opt()
        .expect("system clock out of i64 ns range");
    let current_slot_ns = (now_ns / SLOT_NS) * SLOT_NS;
    let candidate = current_slot_ns + offset_ns;
    let target_ns = if candidate > now_ns {
        candidate
    } else {
        candidate + SLOT_NS
    };
    DateTime::<Utc>::from_timestamp_nanos(target_ns)
}

/// Returns `target - now` clamped to non-negative, as a `std::time::Duration`.
///
/// Suitable for passing to `std::thread::sleep`. Returns zero if `target` is
/// in the past, which makes the caller's "sleep until" loop a no-op.
pub fn duration_until(target: DateTime<Utc>, now: DateTime<Utc>) -> std::time::Duration {
    let delta = target - now;
    delta.to_std().unwrap_or(std::time::Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Build a UTC time at a specific second + nanosecond past a known epoch
    /// minute. Used for deterministic slot-math tests.
    fn at(seconds: f64) -> DateTime<Utc> {
        // Reference: 2026-01-01 00:00:00 UTC. timestamp() = 1767225600,
        // which is divisible by 15 (1767225600 / 15 = 117815040).
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let ns = (seconds * 1_000_000_000.0) as i64;
        base + Duration::nanoseconds(ns)
    }

    #[test]
    fn slot_boundary_aligned_picks_next() {
        // Right at a boundary, with no min_lead, we still advance to the next.
        let now = at(0.0);
        let next = next_slot_start(now, Duration::zero());
        assert_eq!((next - now).num_seconds(), 15);
    }

    #[test]
    fn slot_boundary_sub_second_precision() {
        // 14.999s into a slot — old code (timestamp() % 15) would compute
        // wait=1s and land at 15.999s. We should land at exactly 15.0s.
        let now = at(14.999);
        let next = next_slot_start(now, Duration::zero());
        let delta = (next - at(0.0)).num_milliseconds();
        assert_eq!(delta, 15_000);
    }

    #[test]
    fn slot_boundary_honors_min_lead() {
        // 14.5s into a slot, need 1s of lead → must skip to slot after next.
        let now = at(14.5);
        let next = next_slot_start(now, Duration::seconds(1));
        let delta = (next - at(0.0)).num_milliseconds();
        assert_eq!(delta, 30_000);
    }

    #[test]
    fn slot_boundary_lead_within_current_window() {
        // 5s into a slot, need 1s of lead → next slot at 15s satisfies it.
        let now = at(5.0);
        let next = next_slot_start(now, Duration::seconds(1));
        let delta = (next - at(0.0)).num_milliseconds();
        assert_eq!(delta, 15_000);
    }

    #[test]
    fn audio_start_includes_pre_roll() {
        let now = at(5.0);
        let audio = next_audio_start(now, Duration::zero());
        let delta = (audio - at(0.0)).num_milliseconds();
        // Next slot at 15.0s, audio at 15.5s.
        assert_eq!(delta, 15_500);
    }

    #[test]
    fn audio_start_lead_skips_to_safe_slot() {
        // 14.7s into a slot. Audio at slot+0.5s. If we need 1s of lead
        // from now to audio start, slot at 15.0 only gives 0.8s — must skip.
        let now = at(14.7);
        let audio = next_audio_start(now, Duration::seconds(1));
        let delta = (audio - at(0.0)).num_milliseconds();
        assert_eq!(delta, 30_500);
    }

    #[test]
    fn audio_start_lead_within_pre_roll_is_satisfied() {
        // 14.7s into a slot. Audio at slot+0.5s = 15.5s. If caller only
        // needs 0.3s of lead (less than pre_roll), the next slot's audio
        // start is fine.
        let now = at(14.7);
        let audio = next_audio_start(now, Duration::milliseconds(300));
        let delta = (audio - at(0.0)).num_milliseconds();
        assert_eq!(delta, 15_500);
    }

    #[test]
    fn next_phase_within_current_slot() {
        // 5s in, asking for slot+13 → returns the current slot's :13 mark.
        let now = at(5.0);
        let target = next_phase(now, Duration::seconds(13));
        let delta = (target - at(0.0)).num_milliseconds();
        assert_eq!(delta, 13_000);
    }

    #[test]
    fn next_phase_past_advances_to_next_slot() {
        // 14s in, asking for slot+13 → already past, advance to next slot's :13.
        let now = at(14.0);
        let target = next_phase(now, Duration::seconds(13));
        let delta = (target - at(0.0)).num_milliseconds();
        assert_eq!(delta, 28_000);
    }

    #[test]
    fn next_phase_at_offset_advances() {
        // Right at slot+13.0 — that boundary is not strictly future, advance.
        let now = at(13.0);
        let target = next_phase(now, Duration::seconds(13));
        let delta = (target - at(0.0)).num_milliseconds();
        assert_eq!(delta, 28_000);
    }

    #[test]
    fn next_phase_zero_offset_is_next_slot_start() {
        let now = at(5.0);
        assert_eq!(
            next_phase(now, Duration::zero()),
            next_slot_start(now, Duration::zero())
        );
    }

    #[test]
    #[should_panic(expected = "phase offset must be in [0, 15s)")]
    fn next_phase_rejects_offset_at_slot_length() {
        next_phase(at(0.0), Duration::seconds(15));
    }

    #[test]
    fn duration_until_clamps_negative_to_zero() {
        let now = at(10.0);
        let past = at(5.0);
        assert_eq!(duration_until(past, now), std::time::Duration::ZERO);
    }

    #[test]
    fn duration_until_positive_passthrough() {
        let now = at(0.0);
        let future = at(2.5);
        let d = duration_until(future, now);
        assert_eq!(d.as_millis(), 2_500);
    }

    #[test]
    fn next_slot_with_parity_skips_same_parity() {
        // now = :05 (in even slot 0). Asking for Odd → :15.
        let now = at(5.0);
        let target = next_slot_with_parity(now, SlotParity::Odd);
        let delta = (target - at(0.0)).num_milliseconds();
        assert_eq!(delta, 15_000);
    }

    #[test]
    fn next_slot_with_parity_advances_two_slots_when_current_is_wanted() {
        // now = :05 (even slot 0). Asking for Even → :30 (next even slot,
        // skipping the odd one at :15).
        let now = at(5.0);
        let target = next_slot_with_parity(now, SlotParity::Even);
        let delta = (target - at(0.0)).num_milliseconds();
        assert_eq!(delta, 30_000);
    }

    #[test]
    fn next_slot_with_parity_at_boundary_advances_to_next_match() {
        // now = exactly :15.000 (odd slot start). Even slots are :00, :30...
        // The current slot has already started, so next Odd is :45.
        let now = at(15.0);
        let target = next_slot_with_parity(now, SlotParity::Odd);
        let delta = (target - at(0.0)).num_milliseconds();
        assert_eq!(delta, 45_000);
    }

    #[test]
    fn next_slot_with_parity_inside_wanted_slot_advances() {
        // now = :05 (inside even slot 0). Asking for Even — current slot
        // has already started, must advance. Next even is :30.
        let now = at(5.0);
        let target = next_slot_with_parity(now, SlotParity::Even);
        let delta = (target - at(0.0)).num_milliseconds();
        assert_eq!(delta, 30_000);
    }

    #[test]
    fn slot_parity_even_at_boundary_zero() {
        // 2026-01-01 00:00:00 UTC. timestamp() = 1767225600.
        // 1767225600 / 15 = 117815040 (even index) → Even.
        assert_eq!(SlotParity::of(at(0.0)), SlotParity::Even);
    }

    #[test]
    fn slot_parity_odd_at_boundary_fifteen() {
        // 15s later → 117815041 (odd index) → Odd.
        assert_eq!(SlotParity::of(at(15.0)), SlotParity::Odd);
    }

    #[test]
    fn slot_parity_within_slot_uses_floor() {
        // 14.999s into slot 0 still resolves to that slot's parity.
        assert_eq!(SlotParity::of(at(14.999)), SlotParity::Even);
    }

    #[test]
    fn slot_parity_opposite_invariant() {
        assert_eq!(SlotParity::Even.opposite(), SlotParity::Odd);
        assert_eq!(SlotParity::Odd.opposite(), SlotParity::Even);
        assert_eq!(SlotParity::Even.opposite().opposite(), SlotParity::Even);
    }
}
