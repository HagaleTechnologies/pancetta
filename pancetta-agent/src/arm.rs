//! Armed-TX state machine for the pancetta station agent — the safety crown
//! jewel of the remote-operation path.
//!
//! This module answers exactly one question: **"is remote TX permitted right
//! now?"** The answer is the logical **AND of every safety condition** — never
//! an OR. If *any* gate is closed, TX is denied. Adding a new safety condition
//! can only ever make the gate *more* restrictive.
//!
//! Security model (dispensa ADR-0002 §5):
//! - **Dead-man / heartbeat auto-disarm.** A remote arm must be continuously
//!   refreshed by heartbeats; if the client link goes silent for
//!   [`HEARTBEAT_TIMEOUT_MS`], the station auto-disarms. A stale link can never
//!   keep the transmitter armed.
//! - **TTL.** Every grant carries a finite time-to-live; past it, the arm
//!   expires regardless of heartbeats.
//! - **Local-kill primacy.** The station's local kill switch (maps to
//!   `TxPolicy::Disabled` / Shift+Q at the coordinator) overrides everything.
//!   While engaged, TX is denied even with a fresh, valid, consented arm.
//! - **Local consent gate** (operator decision): a station-side
//!   `remote_tx_enabled` switch, **default OFF**, is ANDed with everything. The
//!   operator at the rig must have explicitly opted in to remote TX.
//! - **Grant scope.** A grant that does not carry TX scope
//!   ([`VerifiedArmGrant::scope_tx`] `== false`) can never permit TX and is
//!   rejected at arm time.
//!
//! Purity:
//! - The state machine is **pure and deterministic**. It never reads a clock;
//!   every method takes `now_ms: i64` (unix milliseconds) from the caller.
//! - It never performs side effects. Each mutating event returns a
//!   `Vec<`[`ArmEffect`]`>` describing what the caller should do (write an audit
//!   record, tell the coordinator it was disarmed). The caller owns IO.
//! - Token verification happens in an earlier phase; [`VerifiedArmGrant`] is
//!   assumed already authenticated.
//!
//! Relationship between [`tick`](ArmState::tick) and
//! [`tx_permitted`](ArmState::tx_permitted):
//! - `tick` is the *mutating* dead-man/TTL sweep: it flips the stored armed flag
//!   off and emits `Disarmed` + audit effects when a deadline passes.
//! - `tx_permitted` is a *pure read* that **independently re-checks the clock**,
//!   so it returns the correct (safe) answer even if `tick` has not yet run for
//!   the current `now`. It never resurrects an expired arm and never mutates.

use crate::audit::{AuditEvent, AuditKind};

/// Dead-man window: if an armed session receives no heartbeat for this many
/// milliseconds, it auto-disarms. TX is denied the instant the window is hit.
pub const HEARTBEAT_TIMEOUT_MS: i64 = 30_000;

/// A grant whose token has already been cryptographically verified in an
/// earlier phase. Here it is treated as trusted input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedArmGrant {
    /// The operator this arm is attributed to (audit + coordinator display).
    pub operator_callsign: String,
    /// Time-to-live for the arm, in milliseconds from the arm instant.
    pub ttl_ms: i64,
    /// Whether the grant carries TX scope. If `false`, the grant can never
    /// permit TX and [`ArmState::arm`] rejects it.
    pub scope_tx: bool,
}

/// Why TX is (not) permitted, for audit `detail` and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxPermit {
    /// All gates open — remote TX is permitted.
    Permitted,
    /// A gate is closed. See [`DenyReason`].
    Denied(DenyReason),
}

/// The first-closed gate that denies TX. Order of evaluation is deterministic
/// (see [`ArmState::tx_permit_reason`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DenyReason {
    /// Not currently armed (fresh state, disarmed, or auto-disarmed).
    NotArmed,
    /// The grant TTL has elapsed.
    Expired,
    /// No heartbeat within [`HEARTBEAT_TIMEOUT_MS`] — dead-man triggered.
    HeartbeatLost,
    /// The station-local consent gate (`remote_tx_enabled`) is OFF.
    NoLocalConsent,
    /// The station-local kill switch is engaged (local-kill primacy).
    LocallyKilled,
    /// The grant does not carry TX scope.
    NoTxScope,
}

impl DenyReason {
    /// A short stable string for audit `detail`.
    pub fn as_str(&self) -> &'static str {
        match self {
            DenyReason::NotArmed => "not-armed",
            DenyReason::Expired => "ttl-expired",
            DenyReason::HeartbeatLost => "heartbeat-lost",
            DenyReason::NoLocalConsent => "no-local-consent",
            DenyReason::LocallyKilled => "locally-killed",
            DenyReason::NoTxScope => "no-tx-scope",
        }
    }
}

/// Reason an armed session was auto-disarmed by [`ArmState::tick`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisarmReason {
    /// The operator (or coordinator) explicitly disarmed.
    OperatorDisarm,
    /// The grant TTL elapsed.
    TtlExpired,
    /// No heartbeat within the dead-man window.
    HeartbeatLost,
}

impl DisarmReason {
    /// A short stable string for audit `detail`.
    pub fn as_str(&self) -> &'static str {
        match self {
            DisarmReason::OperatorDisarm => "operator-disarm",
            DisarmReason::TtlExpired => "ttl-expired",
            DisarmReason::HeartbeatLost => "heartbeat-lost",
        }
    }
}

/// A side effect the caller must perform. The state machine itself is pure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArmEffect {
    /// Append this record to the audit log.
    Audit(AuditEvent),
    /// The session transitioned out of armed; tell the coordinator to stand
    /// down remote TX. Always accompanied by a corresponding `Audit` effect.
    Disarmed {
        /// Why the session disarmed.
        reason: DisarmReason,
    },
}

/// The private "currently armed" record. Absent (`None`) means not armed.
#[derive(Clone, Debug)]
struct ArmedSession {
    operator_callsign: String,
    armed_at_ms: i64,
    ttl_ms: i64,
    last_heartbeat_ms: i64,
    scope_tx: bool,
}

impl ArmedSession {
    /// TTL deadline (exclusive): expired iff `now >= expiry`.
    fn expiry_ms(&self) -> i64 {
        self.armed_at_ms.saturating_add(self.ttl_ms)
    }
}

/// The armed-TX safety state machine.
///
/// Construct with [`ArmState::new`] (not armed, no consent, not killed). Feed it
/// events (`arm` / `heartbeat` / `disarm` / `set_local_consent` /
/// `set_local_kill` / `tick`) and query [`tx_permitted`](Self::tx_permitted).
#[derive(Clone, Debug)]
pub struct ArmState {
    /// `Some` iff currently armed.
    session: Option<ArmedSession>,
    /// Station-local consent gate (`remote_tx_enabled`); **default OFF**.
    local_consent: bool,
    /// Station-local kill switch; **default not engaged**. Overrides everything.
    local_kill: bool,
}

impl Default for ArmState {
    fn default() -> Self {
        Self::new()
    }
}

impl ArmState {
    /// A fresh state: not armed, local consent OFF, not killed.
    pub fn new() -> Self {
        Self {
            session: None,
            local_consent: false,
            local_kill: false,
        }
    }

    // --- read-only accessors (for diagnostics / the coordinator) -----------

    /// Whether an armed session currently exists (ignores time/consent/kill).
    pub fn is_armed(&self) -> bool {
        self.session.is_some()
    }

    /// The station-local consent gate value.
    pub fn local_consent(&self) -> bool {
        self.local_consent
    }

    /// Whether the station-local kill switch is engaged.
    pub fn local_kill(&self) -> bool {
        self.local_kill
    }

    /// The operator the current arm is attributed to, if armed.
    pub fn operator_callsign(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.operator_callsign.as_str())
    }

    // --- events ------------------------------------------------------------

    /// Arm from a verified grant. Records `armed_at = now`, the TTL, operator,
    /// and seeds the heartbeat to `now`.
    ///
    /// If the grant lacks TX scope, the state is **not** armed and a `TxDenied`
    /// audit effect (`NoTxScope`) is returned — an arm that could never permit
    /// TX is refused outright.
    pub fn arm(&mut self, grant: VerifiedArmGrant, now_ms: i64) -> Vec<ArmEffect> {
        if !grant.scope_tx {
            return vec![ArmEffect::Audit(AuditEvent {
                ts_unix_ms: now_ms,
                kind: AuditKind::TxDenied,
                operator_callsign: Some(grant.operator_callsign),
                detail: format!("arm rejected: {}", DenyReason::NoTxScope.as_str()),
            })];
        }

        self.session = Some(ArmedSession {
            operator_callsign: grant.operator_callsign.clone(),
            armed_at_ms: now_ms,
            ttl_ms: grant.ttl_ms,
            last_heartbeat_ms: now_ms,
            scope_tx: true,
        });

        vec![ArmEffect::Audit(AuditEvent {
            ts_unix_ms: now_ms,
            kind: AuditKind::Armed,
            operator_callsign: Some(grant.operator_callsign),
            detail: format!("armed ttl_ms={}", grant.ttl_ms),
        })]
    }

    /// Refresh the dead-man heartbeat. Only meaningful while armed; a heartbeat
    /// received while not armed is ignored (it can never resurrect an arm).
    pub fn heartbeat(&mut self, now_ms: i64) {
        if let Some(s) = self.session.as_mut() {
            // Monotonic guard: never let a stale/replayed lower timestamp shrink
            // the window, but do accept forward progress.
            if now_ms > s.last_heartbeat_ms {
                s.last_heartbeat_ms = now_ms;
            }
        }
    }

    /// Explicit operator/coordinator disarm. No-op (empty effects) if not armed.
    pub fn disarm(&mut self, now_ms: i64) -> Vec<ArmEffect> {
        match self.session.take() {
            Some(s) => vec![
                ArmEffect::Audit(AuditEvent {
                    ts_unix_ms: now_ms,
                    kind: AuditKind::Disarmed,
                    operator_callsign: Some(s.operator_callsign),
                    detail: format!("disarmed: {}", DisarmReason::OperatorDisarm.as_str()),
                }),
                ArmEffect::Disarmed {
                    reason: DisarmReason::OperatorDisarm,
                },
            ],
            None => Vec::new(),
        }
    }

    /// Set the station-local consent gate (`remote_tx_enabled`). Emits a
    /// `LocalConsentChanged` audit effect only on an actual change.
    pub fn set_local_consent(&mut self, enabled: bool, now_ms: i64) -> Vec<ArmEffect> {
        if self.local_consent == enabled {
            return Vec::new();
        }
        self.local_consent = enabled;
        vec![ArmEffect::Audit(AuditEvent {
            ts_unix_ms: now_ms,
            kind: AuditKind::LocalConsentChanged,
            operator_callsign: self.operator_callsign().map(str::to_string),
            detail: format!("local remote_tx_enabled={enabled}"),
        })]
    }

    /// Engage/clear the station-local kill switch (local-kill primacy). Emits a
    /// `LocalKill` audit effect only on an actual change. Engaging the kill does
    /// **not** by itself disarm the session — `tx_permitted` denies while killed,
    /// and the operator's real kill path (`TxPolicy::Disabled`) is separate — but
    /// TX is blocked immediately regardless.
    pub fn set_local_kill(&mut self, engaged: bool, now_ms: i64) -> Vec<ArmEffect> {
        if self.local_kill == engaged {
            return Vec::new();
        }
        self.local_kill = engaged;
        vec![ArmEffect::Audit(AuditEvent {
            ts_unix_ms: now_ms,
            kind: AuditKind::LocalKill,
            operator_callsign: self.operator_callsign().map(str::to_string),
            detail: format!("local_kill engaged={engaged}"),
        })]
    }

    /// Dead-man / TTL sweep. If armed and the grant has expired *or* the
    /// heartbeat window has elapsed, auto-disarm and emit `Disarmed` + an audit
    /// record with the reason. TTL is checked first if both fire.
    pub fn tick(&mut self, now_ms: i64) -> Vec<ArmEffect> {
        let reason = match self.session.as_ref() {
            Some(s) => {
                if now_ms >= s.expiry_ms() {
                    Some(DisarmReason::TtlExpired)
                } else if now_ms.saturating_sub(s.last_heartbeat_ms) >= HEARTBEAT_TIMEOUT_MS {
                    Some(DisarmReason::HeartbeatLost)
                } else {
                    None
                }
            }
            None => None,
        };

        match reason {
            Some(reason) => {
                let s = self.session.take().expect("armed by construction");
                vec![
                    ArmEffect::Audit(AuditEvent {
                        ts_unix_ms: now_ms,
                        kind: AuditKind::Disarmed,
                        operator_callsign: Some(s.operator_callsign),
                        detail: format!("auto-disarm: {}", reason.as_str()),
                    }),
                    ArmEffect::Disarmed { reason },
                ]
            }
            None => Vec::new(),
        }
    }

    // --- the gate ----------------------------------------------------------

    /// **The safety gate.** Returns `true` **iff ALL** of:
    /// currently armed AND `now < armed_at + ttl` AND
    /// `now - last_heartbeat < HEARTBEAT_TIMEOUT_MS` AND `local_consent == true`
    /// AND `local_kill == false` AND the grant carries TX scope.
    ///
    /// Pure read — never mutates, never resurrects an expired arm, and
    /// independently re-checks the clock so it is correct even if `tick` has not
    /// run for this `now`.
    pub fn tx_permitted(&self, now_ms: i64) -> bool {
        matches!(self.tx_permit_reason(now_ms), TxPermit::Permitted)
    }

    /// The same gate as [`tx_permitted`](Self::tx_permitted), but returning the
    /// first-closed gate for audit `detail` / diagnostics. Evaluation order:
    /// NotArmed → NoTxScope → Expired → HeartbeatLost → NoLocalConsent →
    /// LocallyKilled. The *result* is an AND; the order only picks which reason
    /// is reported when several gates are closed.
    pub fn tx_permit_reason(&self, now_ms: i64) -> TxPermit {
        let s = match self.session.as_ref() {
            Some(s) => s,
            None => return TxPermit::Denied(DenyReason::NotArmed),
        };

        // Defense-in-depth: an armed session is only ever created with
        // scope_tx == true, but check anyway so the invariant holds structurally.
        if !s.scope_tx {
            return TxPermit::Denied(DenyReason::NoTxScope);
        }
        if now_ms >= s.expiry_ms() {
            return TxPermit::Denied(DenyReason::Expired);
        }
        if now_ms.saturating_sub(s.last_heartbeat_ms) >= HEARTBEAT_TIMEOUT_MS {
            return TxPermit::Denied(DenyReason::HeartbeatLost);
        }
        if !self.local_consent {
            return TxPermit::Denied(DenyReason::NoLocalConsent);
        }
        if self.local_kill {
            return TxPermit::Denied(DenyReason::LocallyKilled);
        }
        TxPermit::Permitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_000_000; // arbitrary base "now"
    const TTL: i64 = 120_000; // 2 minutes

    fn grant(scope_tx: bool) -> VerifiedArmGrant {
        VerifiedArmGrant {
            operator_callsign: "K5ARH".to_string(),
            ttl_ms: TTL,
            scope_tx,
        }
    }

    /// A helper: armed at T0 with TX scope + consent ON, no kill.
    fn armed_consented() -> ArmState {
        let mut st = ArmState::new();
        st.arm(grant(true), T0);
        st.set_local_consent(true, T0);
        st
    }

    // --- baseline ----------------------------------------------------------

    #[test]
    fn fresh_state_is_not_permitted_not_armed() {
        let st = ArmState::new();
        assert!(!st.tx_permitted(T0));
        assert_eq!(
            st.tx_permit_reason(T0),
            TxPermit::Denied(DenyReason::NotArmed)
        );
        assert!(!st.is_armed());
        assert!(!st.local_consent());
        assert!(!st.local_kill());
    }

    #[test]
    fn armed_with_consent_and_fresh_heartbeat_is_permitted() {
        let st = armed_consented();
        assert!(st.tx_permitted(T0));
        assert_eq!(st.tx_permit_reason(T0), TxPermit::Permitted);
        assert_eq!(st.operator_callsign(), Some("K5ARH"));
    }

    #[test]
    fn arm_without_consent_is_never_permitted() {
        let mut st = ArmState::new();
        st.arm(grant(true), T0);
        // consent still default OFF
        assert!(!st.tx_permitted(T0));
        assert_eq!(
            st.tx_permit_reason(T0),
            TxPermit::Denied(DenyReason::NoLocalConsent)
        );
    }

    // --- heartbeat / dead-man boundary ------------------------------------

    #[test]
    fn heartbeat_boundary_is_exact() {
        let st = armed_consented();
        // Last heartbeat was at T0 (seeded by arm). No further heartbeats.
        let just_before = T0 + HEARTBEAT_TIMEOUT_MS - 1;
        let at_deadline = T0 + HEARTBEAT_TIMEOUT_MS;
        assert!(
            st.tx_permitted(just_before),
            "permitted 1ms before deadline"
        );
        assert!(
            !st.tx_permitted(at_deadline),
            "denied exactly at deadline (>=)"
        );
        assert_eq!(
            st.tx_permit_reason(at_deadline),
            TxPermit::Denied(DenyReason::HeartbeatLost)
        );
    }

    #[test]
    fn a_heartbeat_slides_the_deadman_window() {
        let mut st = armed_consented();
        let hb = T0 + 20_000;
        st.heartbeat(hb);
        // Now the window is measured from `hb`, not T0.
        assert!(st.tx_permitted(hb + HEARTBEAT_TIMEOUT_MS - 1));
        assert!(!st.tx_permitted(hb + HEARTBEAT_TIMEOUT_MS));
    }

    #[test]
    fn heartbeat_while_not_armed_is_ignored() {
        let mut st = ArmState::new();
        st.set_local_consent(true, T0);
        st.heartbeat(T0);
        assert!(!st.tx_permitted(T0));
        assert_eq!(
            st.tx_permit_reason(T0),
            TxPermit::Denied(DenyReason::NotArmed)
        );
    }

    #[test]
    fn stale_heartbeat_cannot_shrink_the_window() {
        let mut st = armed_consented();
        st.heartbeat(T0 + 10_000); // advance
                                   // A replayed/older heartbeat must not move the window backward.
        st.heartbeat(T0 + 5_000);
        assert!(st.tx_permitted(T0 + 10_000 + HEARTBEAT_TIMEOUT_MS - 1));
    }

    // --- TTL boundary ------------------------------------------------------

    #[test]
    fn ttl_boundary_is_exact() {
        // Use a short TTL well within the heartbeat window so TTL is the gate.
        let mut st = ArmState::new();
        let short_ttl = 10_000;
        st.arm(
            VerifiedArmGrant {
                operator_callsign: "K5ARH".into(),
                ttl_ms: short_ttl,
                scope_tx: true,
            },
            T0,
        );
        st.set_local_consent(true, T0);
        assert!(
            st.tx_permitted(T0 + short_ttl - 1),
            "permitted 1ms before ttl"
        );
        assert!(
            !st.tx_permitted(T0 + short_ttl),
            "denied exactly at ttl (>=)"
        );
        assert_eq!(
            st.tx_permit_reason(T0 + short_ttl),
            TxPermit::Denied(DenyReason::Expired)
        );
    }

    // --- local kill --------------------------------------------------------

    #[test]
    fn local_kill_denies_immediately_and_survives_heartbeats() {
        let mut st = armed_consented();
        assert!(st.tx_permitted(T0));
        st.set_local_kill(true, T0);
        assert!(!st.tx_permitted(T0), "killed => denied immediately");
        assert_eq!(
            st.tx_permit_reason(T0),
            TxPermit::Denied(DenyReason::LocallyKilled)
        );
        // Fresh heartbeats cannot un-kill.
        st.heartbeat(T0 + 1_000);
        st.heartbeat(T0 + 2_000);
        assert!(!st.tx_permitted(T0 + 2_000));
        // Only clearing the kill restores permission (still armed + consented).
        st.set_local_kill(false, T0 + 3_000);
        assert!(st.tx_permitted(T0 + 3_000));
    }

    #[test]
    fn kill_engaged_before_arm_blocks_a_later_valid_arm() {
        let mut st = ArmState::new();
        st.set_local_kill(true, T0);
        st.arm(grant(true), T0);
        st.set_local_consent(true, T0);
        assert!(!st.tx_permitted(T0));
        assert_eq!(
            st.tx_permit_reason(T0),
            TxPermit::Denied(DenyReason::LocallyKilled)
        );
    }

    // --- local consent -----------------------------------------------------

    #[test]
    fn revoking_consent_denies_immediately() {
        let mut st = armed_consented();
        assert!(st.tx_permitted(T0));
        st.set_local_consent(false, T0);
        assert!(!st.tx_permitted(T0));
        assert_eq!(
            st.tx_permit_reason(T0),
            TxPermit::Denied(DenyReason::NoLocalConsent)
        );
    }

    // --- scope -------------------------------------------------------------

    #[test]
    fn grant_without_tx_scope_is_rejected_and_never_permits() {
        let mut st = ArmState::new();
        let effects = st.arm(grant(false), T0);
        st.set_local_consent(true, T0);
        assert!(!st.is_armed(), "no-scope grant does not arm");
        assert!(!st.tx_permitted(T0));
        assert_eq!(
            st.tx_permit_reason(T0),
            TxPermit::Denied(DenyReason::NotArmed)
        );
        // The rejection was audited as a TxDenied(NoTxScope).
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            ArmEffect::Audit(ev) => {
                assert_eq!(ev.kind, AuditKind::TxDenied);
                assert!(ev.detail.contains("no-tx-scope"));
            }
            other => panic!("expected audit effect, got {other:?}"),
        }
    }

    // --- no silent resurrection -------------------------------------------

    #[test]
    fn expired_arm_never_resurrects_via_heartbeat() {
        let mut st = ArmState::new();
        let short_ttl = 10_000;
        st.arm(
            VerifiedArmGrant {
                operator_callsign: "K5ARH".into(),
                ttl_ms: short_ttl,
                scope_tx: true,
            },
            T0,
        );
        st.set_local_consent(true, T0);
        let after_expiry = T0 + short_ttl + 5;
        assert!(!st.tx_permitted(after_expiry));
        // A heartbeat after expiry must NOT re-permit — the arm is dead.
        st.heartbeat(after_expiry);
        assert!(!st.tx_permitted(after_expiry));
        assert_eq!(
            st.tx_permit_reason(after_expiry),
            TxPermit::Denied(DenyReason::Expired)
        );
        // Only a fresh arm restores permission.
        st.arm(
            VerifiedArmGrant {
                operator_callsign: "K5ARH".into(),
                ttl_ms: short_ttl,
                scope_tx: true,
            },
            after_expiry,
        );
        assert!(st.tx_permitted(after_expiry));
    }

    #[test]
    fn heartbeat_lost_then_tick_disarms_and_a_later_heartbeat_does_not_resurrect() {
        let mut st = armed_consented();
        let dead = T0 + HEARTBEAT_TIMEOUT_MS;
        // tick past the dead-man window disarms.
        let effects = st.tick(dead);
        assert!(!st.is_armed());
        assert!(effects
            .iter()
            .any(|e| matches!(e, ArmEffect::Disarmed { reason } if *reason == DisarmReason::HeartbeatLost)));
        // A heartbeat now targets no session; still not permitted.
        st.heartbeat(dead);
        assert!(!st.tx_permitted(dead));
        assert_eq!(
            st.tx_permit_reason(dead),
            TxPermit::Denied(DenyReason::NotArmed)
        );
    }

    // --- tick effects ------------------------------------------------------

    #[test]
    fn tick_before_any_deadline_is_a_noop() {
        let mut st = armed_consented();
        assert!(st.tick(T0 + 5_000).is_empty());
        assert!(st.is_armed());
    }

    #[test]
    fn tick_past_ttl_emits_disarmed_and_audit_with_ttl_reason() {
        let mut st = ArmState::new();
        st.arm(
            VerifiedArmGrant {
                operator_callsign: "K5ARH".into(),
                ttl_ms: 10_000,
                scope_tx: true,
            },
            T0,
        );
        st.set_local_consent(true, T0);
        let effects = st.tick(T0 + 10_000);
        assert!(!st.is_armed());
        assert_eq!(effects.len(), 2);
        match &effects[0] {
            ArmEffect::Audit(ev) => {
                assert_eq!(ev.kind, AuditKind::Disarmed);
                assert!(ev.detail.contains("ttl-expired"));
                assert_eq!(ev.operator_callsign.as_deref(), Some("K5ARH"));
            }
            other => panic!("expected audit, got {other:?}"),
        }
        assert_eq!(
            effects[1],
            ArmEffect::Disarmed {
                reason: DisarmReason::TtlExpired
            }
        );
    }

    #[test]
    fn tick_past_heartbeat_emits_disarmed_and_audit_with_heartbeat_reason() {
        let mut st = armed_consented();
        let effects = st.tick(T0 + HEARTBEAT_TIMEOUT_MS);
        assert!(!st.is_armed());
        match &effects[0] {
            ArmEffect::Audit(ev) => {
                assert_eq!(ev.kind, AuditKind::Disarmed);
                assert!(ev.detail.contains("heartbeat-lost"));
            }
            other => panic!("expected audit, got {other:?}"),
        }
        assert_eq!(
            effects[1],
            ArmEffect::Disarmed {
                reason: DisarmReason::HeartbeatLost
            }
        );
    }

    #[test]
    fn ttl_takes_precedence_over_heartbeat_when_both_fire() {
        // TTL shorter than heartbeat window; tick well past both.
        let mut st = ArmState::new();
        st.arm(
            VerifiedArmGrant {
                operator_callsign: "K5ARH".into(),
                ttl_ms: 5_000,
                scope_tx: true,
            },
            T0,
        );
        st.set_local_consent(true, T0);
        let effects = st.tick(T0 + 40_000); // past ttl AND past heartbeat window
        assert_eq!(
            effects[1],
            ArmEffect::Disarmed {
                reason: DisarmReason::TtlExpired
            }
        );
    }

    #[test]
    fn explicit_disarm_emits_effects_then_is_noop() {
        let mut st = armed_consented();
        let effects = st.disarm(T0 + 1_000);
        assert_eq!(effects.len(), 2);
        assert_eq!(
            effects[1],
            ArmEffect::Disarmed {
                reason: DisarmReason::OperatorDisarm
            }
        );
        assert!(!st.is_armed());
        assert!(!st.tx_permitted(T0 + 1_000));
        // Disarm again = no effects.
        assert!(st.disarm(T0 + 2_000).is_empty());
    }

    // --- consent/kill audit-emission edges --------------------------------

    #[test]
    fn consent_and_kill_only_audit_on_change() {
        let mut st = ArmState::new();
        // OFF -> OFF: no effect.
        assert!(st.set_local_consent(false, T0).is_empty());
        // OFF -> ON: one effect.
        assert_eq!(st.set_local_consent(true, T0).len(), 1);
        // ON -> ON: no effect.
        assert!(st.set_local_consent(true, T0).is_empty());
        // kill: same pattern.
        assert!(st.set_local_kill(false, T0).is_empty());
        assert_eq!(st.set_local_kill(true, T0).len(), 1);
        assert!(st.set_local_kill(true, T0).is_empty());
    }

    // --- property / invariant test ----------------------------------------

    /// Deterministic 64-bit LCG (no `rand`, no clock). Fixed seed => stable.
    struct Lcg(u64);
    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            // Numerical Recipes constants.
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n
        }
    }

    /// A shadow model of every gate, computed independently of `ArmState`'s
    /// internals, to cross-check the safety invariant:
    /// permitted ⇒ armed ∧ scope ∧ unexpired ∧ heartbeat-fresh ∧ consent ∧ ¬killed.
    #[derive(Clone)]
    struct Shadow {
        armed: bool,
        armed_at: i64,
        ttl: i64,
        last_hb: i64,
        scope_tx: bool,
        consent: bool,
        kill: bool,
    }

    impl Shadow {
        fn new() -> Self {
            Shadow {
                armed: false,
                armed_at: 0,
                ttl: 0,
                last_hb: 0,
                scope_tx: false,
                consent: false,
                kill: false,
            }
        }
        fn any_gate_closed(&self, now: i64) -> bool {
            !self.armed
                || !self.scope_tx
                || now >= self.armed_at.saturating_add(self.ttl)
                || now.saturating_sub(self.last_hb) >= HEARTBEAT_TIMEOUT_MS
                || !self.consent
                || self.kill
        }
    }

    #[test]
    fn property_permitted_implies_all_gates_open() {
        let mut rng = Lcg(0x5151_5151_ABCD_1234);
        // Many independent trajectories.
        for _ in 0..2_000 {
            let mut st = ArmState::new();
            let mut sh = Shadow::new();
            let mut now: i64 = 1_000_000;

            for _ in 0..40 {
                // Advance time by a bounded random amount (can cross deadlines).
                now += (rng.below(40_000)) as i64;

                match rng.below(6) {
                    0 => {
                        // arm (random scope, random ttl in [1ms, 90s])
                        let scope = rng.below(2) == 1;
                        let ttl = 1 + rng.below(90_000) as i64;
                        st.arm(
                            VerifiedArmGrant {
                                operator_callsign: "K5ARH".into(),
                                ttl_ms: ttl,
                                scope_tx: scope,
                            },
                            now,
                        );
                        // Shadow: arm only takes effect if scope_tx.
                        if scope {
                            sh.armed = true;
                            sh.armed_at = now;
                            sh.ttl = ttl;
                            sh.last_hb = now;
                            sh.scope_tx = true;
                        }
                        // (no-scope arm leaves prior session untouched, matching impl)
                    }
                    1 => {
                        st.heartbeat(now);
                        if sh.armed && now > sh.last_hb {
                            sh.last_hb = now;
                        }
                    }
                    2 => {
                        st.disarm(now);
                        sh.armed = false;
                    }
                    3 => {
                        let en = rng.below(2) == 1;
                        st.set_local_consent(en, now);
                        sh.consent = en;
                    }
                    4 => {
                        let en = rng.below(2) == 1;
                        st.set_local_kill(en, now);
                        sh.kill = en;
                    }
                    _ => {
                        st.tick(now);
                        // Shadow tick: auto-disarm if past ttl or heartbeat window.
                        if sh.armed
                            && (now >= sh.armed_at.saturating_add(sh.ttl)
                                || now.saturating_sub(sh.last_hb) >= HEARTBEAT_TIMEOUT_MS)
                        {
                            sh.armed = false;
                        }
                    }
                }

                // Query at `now` and at a few future offsets (tx_permitted must be
                // correct even without a tick at that instant).
                for dt in [
                    0i64,
                    1,
                    HEARTBEAT_TIMEOUT_MS - 1,
                    HEARTBEAT_TIMEOUT_MS,
                    200_000,
                ] {
                    let q = now + dt;
                    let permitted = st.tx_permitted(q);

                    // THE SAFETY INVARIANT: if permitted, no gate may be closed.
                    if permitted {
                        assert!(
                            !sh.any_gate_closed(q),
                            "SAFETY VIOLATION: tx_permitted(true) with a closed gate. \
                             shadow: armed={} scope={} armed_at={} ttl={} last_hb={} \
                             consent={} kill={} q={}",
                            sh.armed,
                            sh.scope_tx,
                            sh.armed_at,
                            sh.ttl,
                            sh.last_hb,
                            sh.consent,
                            sh.kill,
                            q
                        );
                    }
                    // And the contrapositive against the impl's own reason: a
                    // Permitted verdict must agree the boolean is true.
                    let reason = st.tx_permit_reason(q);
                    assert_eq!(permitted, matches!(reason, TxPermit::Permitted));
                }
            }
        }
    }
}
