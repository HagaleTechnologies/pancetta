//! FT8 Fox-mode slot geometry (PAN-386).
//!
//! A WSJT-X Fox transmits every stream on a fixed 60 Hz grid low in the
//! passband (`foxgen.f90`, `fstep=60`), so Hounds can call anywhere above
//! 1000 Hz while the Fox's own signals stay out of their way. Pancetta's Fox
//! mode uses the same grid: the Fox CQ and every Hound answer take the lowest
//! free slot `300 + 60k` Hz (k = 0..10, never above 900 Hz) and keep it for the
//! life of the QSO.
//!
//! Both the coordinator (choosing a slot) and the TX worker (spacing a Fox
//! bundle) read these constants, so they live here rather than in either.

use crate::qso_manager::MIN_TX_SEPARATION_HZ;

/// Lowest Fox slot, in Hz of audio offset.
pub const FOX_SLOT_BASE_HZ: f64 = 300.0;

/// Distance between adjacent Fox slots, in Hz (WSJT-X `foxgen.f90` `fstep`).
pub const FOX_SLOT_STEP_HZ: f64 = 60.0;

/// Highest Fox slot, in Hz of audio offset.
pub const FOX_SLOT_MAX_HZ: f64 = 900.0;

/// Minimum separation between two Fox-role streams, in Hz.
///
/// One slot step less 1 Hz of float slack. FT8 occupies 50 Hz, so this leaves
/// a 9 Hz guard between adjacent Fox frames. Any pair that includes a
/// non-Fox stream keeps [`MIN_TX_SEPARATION_HZ`].
pub const FOX_MIN_SEPARATION_HZ: f64 = FOX_SLOT_STEP_HZ - 1.0;

/// Every Fox slot, lowest first: 300, 360, ..., 900 Hz.
pub fn fox_slots() -> impl Iterator<Item = f64> {
    let count = ((FOX_SLOT_MAX_HZ - FOX_SLOT_BASE_HZ) / FOX_SLOT_STEP_HZ).round() as u32 + 1;
    (0..count).map(|k| FOX_SLOT_BASE_HZ + FOX_SLOT_STEP_HZ * f64::from(k))
}

/// The lowest free Fox slot, or `None` when every slot is taken.
///
/// A slot is free when it is at least [`FOX_MIN_SEPARATION_HZ`] from every
/// offset in `fox_offsets` (other Fox-role streams) and at least
/// [`MIN_TX_SEPARATION_HZ`] from every offset in `other_offsets` (every other
/// active stream).
pub fn fox_slot_offset(fox_offsets: &[f64], other_offsets: &[f64]) -> Option<f64> {
    fox_slots().find(|&slot| {
        fox_offsets
            .iter()
            .all(|&f| (slot - f).abs() >= FOX_MIN_SEPARATION_HZ)
            && other_offsets
                .iter()
                .all(|&f| (slot - f).abs() >= MIN_TX_SEPARATION_HZ)
    })
}

/// Whether the Fox slot geometry applies in `active_mode`.
///
/// WSJT-X Fox/Hound is FT8 only, so an FT4 or FT2 Fox keeps the legacy
/// geometry (CQ at 1500 Hz, Tx=Rx answers).
pub fn fox_slots_apply(active_mode: &str) -> bool {
    active_mode.eq_ignore_ascii_case("FT8")
}

#[cfg(test)]
#[path = "fox/tests/fox_slot_tests.rs"]
mod fox_slot_tests;
