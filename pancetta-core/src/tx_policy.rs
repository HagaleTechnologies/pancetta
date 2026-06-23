//! Tri-state global TX policy.
//!
//! The operator's master TX-enable switch has three states, ordered from
//! most permissive to least:
//!
//! 1. [`TxPolicy::Full`] — initiate AND respond normally (the default, and
//!    the historical behavior before this policy existed).
//! 2. [`TxPolicy::RespondOnly`] — do NOT initiate any new transmissions
//!    (no calling CQ, no hunting/pouncing on stations calling CQ), but
//!    continue to answer stations calling *us* and keep transmitting for
//!    QSOs already in progress.
//! 3. [`TxPolicy::Disabled`] — no TX at all; receive only. This is the
//!    hard mute enforced at the TX execution layer (PTT/audio/modulate).
//!
//! The coordinator stores this in an [`std::sync::atomic::AtomicU8`] so the
//! TX worker and decision loops can read it cheaply on every cycle. Use
//! [`TxPolicy::as_u8`] / [`TxPolicy::from_u8`] for the atomic round-trip.

/// Global, operator-controlled TX policy with three states.
///
/// See the [module docs](self) for the full semantics of each state.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum TxPolicy {
    /// Initiate and respond normally — the default. Everything the station
    /// can transmit is allowed (calling CQ, hunting CQers, answering
    /// callers, QSO-in-progress messages, manual sends, tune).
    #[default]
    Full,
    /// Respond-only: suppress all *initiations* (no new CQ, no
    /// hunting/calling stations that are calling CQ), but still answer
    /// stations calling us and keep transmitting for QSOs already in
    /// progress.
    RespondOnly,
    /// No TX at all — receive only. Enforced as a hard mute at the TX
    /// execution layer: PTT is never keyed, no audio is modulated or
    /// played, the request is consumed and reported as blocked.
    Disabled,
}

impl TxPolicy {
    /// `true` when the policy allows *initiating* a new transmission —
    /// calling CQ, or hunting/pouncing on a station that is calling CQ.
    /// Only [`TxPolicy::Full`] permits initiation.
    pub fn allows_initiation(&self) -> bool {
        matches!(self, TxPolicy::Full)
    }

    /// `true` when the policy allows *any* transmission to reach the air —
    /// both [`TxPolicy::Full`] and [`TxPolicy::RespondOnly`]. Only
    /// [`TxPolicy::Disabled`] returns `false`. Used by the hard mute at the
    /// TX execution layer.
    pub fn allows_any_tx(&self) -> bool {
        matches!(self, TxPolicy::Full | TxPolicy::RespondOnly)
    }

    /// Stable `u8` encoding for atomic storage. The mapping is fixed and
    /// MUST NOT change (`0` = Full, `1` = RespondOnly, `2` = Disabled).
    pub fn as_u8(&self) -> u8 {
        match self {
            TxPolicy::Full => 0,
            TxPolicy::RespondOnly => 1,
            TxPolicy::Disabled => 2,
        }
    }

    /// Decode a [`TxPolicy`] from its stable `u8` encoding (see
    /// [`TxPolicy::as_u8`]). Any unrecognized value decodes to the safe
    /// default [`TxPolicy::Full`] — callers writing the atomic only ever
    /// store values produced by `as_u8`, so this branch is defensive.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => TxPolicy::Full,
            1 => TxPolicy::RespondOnly,
            2 => TxPolicy::Disabled,
            _ => TxPolicy::Full,
        }
    }

    /// Cycle to the next policy in the Full → RespondOnly → Disabled → Full
    /// order. Drives the operator's single-key toggle.
    pub fn cycle(&self) -> Self {
        match self {
            TxPolicy::Full => TxPolicy::RespondOnly,
            TxPolicy::RespondOnly => TxPolicy::Disabled,
            TxPolicy::Disabled => TxPolicy::Full,
        }
    }

    /// Short, human-readable label for the UI banner / logs.
    pub fn label(&self) -> &'static str {
        match self {
            TxPolicy::Full => "FULL",
            TxPolicy::RespondOnly => "RESPOND-ONLY",
            TxPolicy::Disabled => "DISABLED",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_full() {
        assert_eq!(TxPolicy::default(), TxPolicy::Full);
    }

    #[test]
    fn initiation_only_full() {
        assert!(TxPolicy::Full.allows_initiation());
        assert!(!TxPolicy::RespondOnly.allows_initiation());
        assert!(!TxPolicy::Disabled.allows_initiation());
    }

    #[test]
    fn any_tx_full_and_respond_only() {
        assert!(TxPolicy::Full.allows_any_tx());
        assert!(TxPolicy::RespondOnly.allows_any_tx());
        assert!(!TxPolicy::Disabled.allows_any_tx());
    }

    #[test]
    fn u8_roundtrip_stable() {
        for p in [TxPolicy::Full, TxPolicy::RespondOnly, TxPolicy::Disabled] {
            assert_eq!(TxPolicy::from_u8(p.as_u8()), p);
        }
        // Fixed encoding contract.
        assert_eq!(TxPolicy::Full.as_u8(), 0);
        assert_eq!(TxPolicy::RespondOnly.as_u8(), 1);
        assert_eq!(TxPolicy::Disabled.as_u8(), 2);
        // Unknown decodes to the safe default.
        assert_eq!(TxPolicy::from_u8(99), TxPolicy::Full);
    }

    #[test]
    fn cycle_wraps() {
        assert_eq!(TxPolicy::Full.cycle(), TxPolicy::RespondOnly);
        assert_eq!(TxPolicy::RespondOnly.cycle(), TxPolicy::Disabled);
        assert_eq!(TxPolicy::Disabled.cycle(), TxPolicy::Full);
    }
}
