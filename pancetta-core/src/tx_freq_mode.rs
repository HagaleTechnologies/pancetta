//! TX-frequency selection mode.
//!
//! Controls whether pancetta is free to choose / change its TX audio offset
//! on its own, or must hold the offset the operator picked.
//!
//! 1. [`TxFreqMode::Hold`] — the DEFAULT. Once the operator picks a TX offset
//!    it stays put: pancetta never moves it autonomously. The stuck-DX hop,
//!    the autonomous collision-listen jitter, and the autonomous smart-frequency
//!    allocator are all suppressed; autonomous QSOs transmit on the operator's
//!    pinned offset. The operator changes the offset only by explicit action
//!    (`[` / `]` / arrows, or the `t` auto-picker, which chooses a clear offset
//!    once and pins it).
//! 2. [`TxFreqMode::Auto`] — pancetta is free to choose and adjust the TX
//!    offset: the smart-frequency allocator picks per QSO, the collision-listen
//!    jitter moves off interferers when idle, and the stuck-DX detector hops
//!    the offset when a QSO's DX repeats without advancing (likely collision).
//!
//! The coordinator stores this in an [`std::sync::atomic::AtomicU8`] so the QSO
//! engine and the autonomous decision loop can read it cheaply. Use
//! [`TxFreqMode::as_u8`] / [`TxFreqMode::from_u8`] for the atomic round-trip.

/// Operator-controlled TX-frequency selection mode. See the [module
/// docs](self) for the full semantics of each state.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum TxFreqMode {
    /// Hold the operator-picked TX offset; pancetta never moves it on its own.
    /// The default.
    #[default]
    Hold,
    /// Pancetta is free to choose and adjust the TX offset (smart allocator +
    /// collision-jitter + stuck-DX hop all active).
    Auto,
}

impl TxFreqMode {
    /// `true` when pancetta may change the TX offset on its own (Auto mode).
    pub fn allows_auto_change(&self) -> bool {
        matches!(self, TxFreqMode::Auto)
    }

    /// Stable `u8` encoding for atomic storage. The mapping is fixed and MUST
    /// NOT change (`0` = Hold, `1` = Auto).
    pub fn as_u8(&self) -> u8 {
        match self {
            TxFreqMode::Hold => 0,
            TxFreqMode::Auto => 1,
        }
    }

    /// Decode a [`TxFreqMode`] from its stable `u8` encoding (see
    /// [`TxFreqMode::as_u8`]). Any unrecognized value decodes to the safe
    /// default [`TxFreqMode::Hold`].
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => TxFreqMode::Auto,
            _ => TxFreqMode::Hold,
        }
    }

    /// Toggle Hold ↔ Auto. Drives the operator's single-key switch.
    pub fn toggle(&self) -> Self {
        match self {
            TxFreqMode::Hold => TxFreqMode::Auto,
            TxFreqMode::Auto => TxFreqMode::Hold,
        }
    }

    /// Short, human-readable label for the UI chip / logs.
    pub fn label(&self) -> &'static str {
        match self {
            TxFreqMode::Hold => "HOLD",
            TxFreqMode::Auto => "AUTO",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_hold() {
        assert_eq!(TxFreqMode::default(), TxFreqMode::Hold);
    }

    #[test]
    fn auto_allows_change_hold_does_not() {
        assert!(TxFreqMode::Auto.allows_auto_change());
        assert!(!TxFreqMode::Hold.allows_auto_change());
    }

    #[test]
    fn u8_roundtrip_stable() {
        for m in [TxFreqMode::Hold, TxFreqMode::Auto] {
            assert_eq!(TxFreqMode::from_u8(m.as_u8()), m);
        }
        assert_eq!(TxFreqMode::Hold.as_u8(), 0);
        assert_eq!(TxFreqMode::Auto.as_u8(), 1);
        // Unknown decodes to the safe default.
        assert_eq!(TxFreqMode::from_u8(99), TxFreqMode::Hold);
    }

    #[test]
    fn toggle_flips() {
        assert_eq!(TxFreqMode::Hold.toggle(), TxFreqMode::Auto);
        assert_eq!(TxFreqMode::Auto.toggle(), TxFreqMode::Hold);
    }
}
