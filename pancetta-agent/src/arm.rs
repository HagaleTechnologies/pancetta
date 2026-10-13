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
//!   keep the transmitter armed. Each heartbeat is bound to its arm: it must
//!   name the current arm's `jti` (`armJti`) and carry a per-arm-**monotonic**
//!   `seq`. A **replayed** or wrong-arm heartbeat is rejected and does NOT slide
//!   the window (contract `$defs.txHeartbeat`), so a captured heartbeat can
//!   never hold an arm open past its dead-man deadline.
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
//! - **Fresh arm after loss.** Once a deadline (dead-man or TTL) has passed,
//!   the arm is gone for good: a later heartbeat — even one naming the right
//!   `armJti` with a fresh `seq` — first expires the session and is then
//!   ignored. Only a newly signed, previously unseen grant re-arms.
//!
//! Timing (PAN-97):
//! - Local liveness (dead-man silence, the arm's maximum lifetime) is measured
//!   on the **monotonic** clock ([`ArmTime::monotonic`]), so a wall-clock step
//!   can never stretch the heartbeat window.
//! - The grant's signed absolute deadline (`armedUntil`) is checked against
//!   **wall** time ([`ArmTime::unix_ms`]). Either clock expiring denies TX.
//! - `heartbeatIntervalSec` is the client's *sending* cadence ([5,15] s). The
//!   station's silence limit is the fixed [`HEARTBEAT_TIMEOUT_MS`] for every
//!   accepted cadence; the cadence is carried for audit, not used as a timeout.
//!
//! Purity:
//! - The state machine is **pure and deterministic**. It never reads a clock;
//!   every timing-sensitive method takes an [`ArmTime`] sample from the caller
//!   (audit-only setters take unix milliseconds).
//! - It never performs side effects. Each mutating event returns a
//!   `Vec<`[`ArmEffect`]`>` describing what the caller should do (write an audit
//!   record, tell the coordinator it was disarmed). The caller owns IO.
//! - Token verification happens in an earlier phase; [`VerifiedArmGrant`] is
//!   assumed already authenticated.
//!
//! Relationship between [`tick`](ArmState::tick) and
//! [`tx_permitted`](ArmState::tx_permitted):
//! - `tick` is the *mutating* dead-man/TTL sweep: it flips the stored armed flag
//!   off and emits `Disarmed` + audit effects when a deadline passes. The
//!   station session loop runs it on every receive iteration, and
//!   [`heartbeat`](ArmState::heartbeat) runs it before accepting anything.
//! - `tx_permitted` is a *pure read* that **independently re-checks the clock**,
//!   so it returns the correct (safe) answer even if `tick` has not yet run for
//!   the current `now`. It never resurrects an expired arm and never mutates.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::audit::{AuditEvent, AuditKind};

/// Dead-man window: if an armed session receives no heartbeat for this many
/// milliseconds, it auto-disarms. TX is denied the instant the window is hit.
/// This is the station's silence limit for every accepted
/// `heartbeatIntervalSec` (5–15 s sending cadence); it is not derived from
/// the cadence.
pub const HEARTBEAT_TIMEOUT_MS: i64 = 30_000;

/// [`HEARTBEAT_TIMEOUT_MS`] as a monotonic duration.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(HEARTBEAT_TIMEOUT_MS as u64);

/// One clock sample for arm timing: wall time for the signed absolute expiry
/// and audit timestamps, monotonic time for local liveness. Take it as late
/// as possible (after acquiring the arm lock) so a wait cannot make it stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArmTime {
    /// Unix milliseconds (wall clock).
    pub unix_ms: i64,
    /// Monotonic instant taken alongside `unix_ms`.
    pub monotonic: Instant,
}

impl ArmTime {
    /// Sample both clocks now.
    pub fn now() -> Self {
        let monotonic = Instant::now();
        let unix_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
            Err(e) => i64::try_from(e.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
        };
        Self { unix_ms, monotonic }
    }
}

/// A grant whose token has already been cryptographically verified in an
/// earlier phase. Here it is treated as trusted input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedArmGrant {
    /// The operator this arm is attributed to (audit + coordinator display).
    pub operator_callsign: String,
    /// Verified lifetime cap for the arm, in milliseconds (`armedUntil` minus
    /// the verification instant). [`ArmState::arm`] re-measures the remaining
    /// time to [`Self::armed_until_ms`] at acceptance and uses the smaller.
    pub ttl_ms: i64,
    /// The signed absolute arm deadline (`txArmGrant.armedUntil`, unix ms),
    /// checked against wall time for the life of the arm.
    pub armed_until_ms: i64,
    /// The verified heartbeat sending cadence (`heartbeatIntervalSec`, 5–15 s).
    /// The station's silence limit stays [`HEARTBEAT_TIMEOUT_MS`] regardless.
    pub heartbeat_interval_sec: i64,
    /// Whether the grant carries TX scope. If `false`, the grant can never
    /// permit TX and [`ArmState::arm`] rejects it.
    pub scope_tx: bool,
    /// The grant's unique id (`txArmGrant.jti`). Heartbeats must name this exact
    /// arm via `armJti` (contract `$defs.txHeartbeat.armJti`); a heartbeat for a
    /// different `jti` is rejected without sliding the dead-man window.
    pub jti: String,
    /// The client keyId this grant was verified for. Stored on the armed
    /// session (see [`ArmState::armed_client_key_id`]) so a later live
    /// authorization refresh (CQD-11 / dispensa Q-0052) can tell whether the
    /// CURRENT arm's controller is still TX-arm-eligible, independent of the
    /// ham-radio `operator_callsign` attribution above.
    pub client_key_id: String,
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
    /// The station-agent component terminated unexpectedly.
    ComponentCrash,
    /// A live cqdx authorization refresh (CQD-11 / dispensa Q-0052) removed
    /// the current arm's controller from TX-arm eligibility — either the
    /// edge was revoked entirely or it became delegated. The cloud may only
    /// ever REDUCE station-local TX authority (frozen e2e-auth.v1
    /// §6-revocation), so a poll that demotes an already-armed controller
    /// must retroactively disarm rather than leave the live grant to run out
    /// its TTL.
    AuthorizationRevoked,
}

impl DisarmReason {
    /// A short stable string for audit `detail`.
    pub fn as_str(&self) -> &'static str {
        match self {
            DisarmReason::OperatorDisarm => "operator-disarm",
            DisarmReason::TtlExpired => "ttl-expired",
            DisarmReason::HeartbeatLost => "heartbeat-lost",
            DisarmReason::ComponentCrash => "component-crash",
            DisarmReason::AuthorizationRevoked => "authorization-revoked",
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
    /// A heartbeat was **rejected** (wrong `armJti`, or a non-monotonic /
    /// replayed `seq`). The dead-man window was NOT slid; the arm is unchanged.
    /// Accompanied by a corresponding `Audit` (`TxDenied`) effect so a replay
    /// attempt is visible to the auditor.
    HeartbeatRejected {
        /// Why the heartbeat was rejected (stable short string).
        reason: &'static str,
    },
}

/// The private "currently armed" record. Absent (`None`) means not armed.
#[derive(Clone, Debug)]
struct ArmedSession {
    operator_callsign: String,
    /// Monotonic instant the arm was accepted.
    armed_at: Instant,
    /// Maximum monotonic lifetime: the smaller of the verified `ttl_ms` and
    /// the time left to `armed_until_ms` when the arm was accepted.
    lifetime: Duration,
    /// Signed absolute deadline (unix ms), checked against wall time.
    armed_until_ms: i64,
    /// Monotonic instant of the last accepted heartbeat (seeded by `arm`).
    last_heartbeat: Instant,
    /// Verified sending cadence (audit only; see [`HEARTBEAT_TIMEOUT_MS`]).
    heartbeat_interval_sec: i64,
    scope_tx: bool,
    /// The grant's `jti` — the arm this session represents. A heartbeat's
    /// `armJti` must equal this or it is rejected (contract `$defs.txHeartbeat`).
    current_jti: String,
    /// The client keyId this arm was verified for (see
    /// [`ArmState::armed_client_key_id`]).
    client_key_id: String,
    /// The highest heartbeat `seq` accepted for this arm. A heartbeat with
    /// `seq <= last_heartbeat_seq` is a replay/non-monotonic frame and is
    /// rejected without sliding the window. `None` until the first heartbeat.
    last_heartbeat_seq: Option<u64>,
}

impl ArmedSession {
    /// The deadline that has passed at `now`, if any. TTL (wall `armedUntil`
    /// or monotonic lifetime) wins over heartbeat loss. Deadlines are
    /// exclusive (`>=`). A monotonic sample earlier than a stored instant
    /// cannot be measured, so it counts as expired (fail closed).
    fn expired_reason(&self, now: ArmTime) -> Option<DisarmReason> {
        let lifetime_over = now
            .monotonic
            .checked_duration_since(self.armed_at)
            .is_none_or(|elapsed| elapsed >= self.lifetime);
        if now.unix_ms >= self.armed_until_ms || lifetime_over {
            return Some(DisarmReason::TtlExpired);
        }
        let silence_over = now
            .monotonic
            .checked_duration_since(self.last_heartbeat)
            .is_none_or(|silence| silence >= HEARTBEAT_TIMEOUT);
        silence_over.then_some(DisarmReason::HeartbeatLost)
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

    /// The `jti` of the currently-armed grant, if armed. Used by the station
    /// agent to sanity-match a `txDisarm.armJti` against the live arm (a
    /// mismatch still disarms — disarm-any is fail-safe — but is logged).
    pub fn current_arm_jti(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.current_jti.as_str())
    }

    /// The client keyId of the currently-armed grant, if armed (CQD-11 /
    /// dispensa Q-0052). This is the cryptographic client identity the grant
    /// was verified for — distinct from [`Self::operator_callsign`], which
    /// is the ham-radio callsign attribution. Used by the Q-0043
    /// authorization-poll task to detect that a live cqdx refresh has
    /// removed the current controller from TX-arm eligibility (revoked or
    /// newly delegated), so the arm can be disarmed immediately rather than
    /// left to run out its TTL.
    pub fn armed_client_key_id(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.client_key_id.as_str())
    }

    // --- events ------------------------------------------------------------

    /// Arm from a verified grant. Records the accept instant, the effective
    /// lifetime, the signed `armedUntil`, operator and cadence, and seeds the
    /// heartbeat to `now`.
    ///
    /// The effective lifetime is the smaller of the verified `ttl_ms` and the
    /// time left to `armedUntil` at `now`, so a delay between verification and
    /// acceptance (or a wall-clock step in between) cannot extend the grant.
    ///
    /// If the grant lacks TX scope, or its signed deadline has already passed
    /// at `now`, the state is **not** changed and a `TxDenied` audit effect is
    /// returned — an arm that could never permit TX is refused outright.
    pub fn arm(&mut self, grant: VerifiedArmGrant, now: ArmTime) -> Vec<ArmEffect> {
        if !grant.scope_tx {
            return vec![ArmEffect::Audit(AuditEvent {
                ts_unix_ms: now.unix_ms,
                kind: AuditKind::TxDenied,
                operator_callsign: Some(grant.operator_callsign),
                detail: format!("arm rejected: {}", DenyReason::NoTxScope.as_str()),
            })];
        }

        let lifetime_ms = grant
            .ttl_ms
            .min(grant.armed_until_ms.saturating_sub(now.unix_ms));
        if lifetime_ms <= 0 {
            return vec![ArmEffect::Audit(AuditEvent {
                ts_unix_ms: now.unix_ms,
                kind: AuditKind::TxDenied,
                operator_callsign: Some(grant.operator_callsign),
                detail: format!("arm rejected: {}", DenyReason::Expired.as_str()),
            })];
        }

        let session = ArmedSession {
            operator_callsign: grant.operator_callsign,
            armed_at: now.monotonic,
            lifetime: Duration::from_millis(lifetime_ms.unsigned_abs()),
            armed_until_ms: grant.armed_until_ms,
            last_heartbeat: now.monotonic,
            heartbeat_interval_sec: grant.heartbeat_interval_sec,
            scope_tx: true,
            current_jti: grant.jti,
            client_key_id: grant.client_key_id,
            // A fresh arm resets the heartbeat sequence — a new arm's low seq is
            // accepted even if a prior arm had reached a high seq.
            last_heartbeat_seq: None,
        };
        let audit = AuditEvent {
            ts_unix_ms: now.unix_ms,
            kind: AuditKind::Armed,
            operator_callsign: Some(session.operator_callsign.clone()),
            detail: format!(
                "armed ttl_ms={lifetime_ms} heartbeat_interval_sec={} heartbeat_timeout_ms={HEARTBEAT_TIMEOUT_MS}",
                session.heartbeat_interval_sec
            ),
        };
        self.session = Some(session);
        vec![ArmEffect::Audit(audit)]
    }

    /// Refresh the dead-man heartbeat, bound to the arm it names.
    ///
    /// Contract (`$defs.txHeartbeat`): a heartbeat carries the `armJti` of the
    /// arm it keeps alive and a per-arm-monotonic `seq`. This method enforces
    /// both so a **replayed** heartbeat can never hold an arm open past its
    /// dead-man window:
    ///
    /// - A deadline (dead-man or TTL) has passed at `now` → the arm is expired
    ///   first, exactly as [`tick`](Self::tick) would, and the heartbeat is
    ///   then ignored: returns the `Disarmed` + audit effects. This runs before
    ///   any `armJti`/`seq` check, so nothing can refresh a lapsed arm; only a
    ///   fresh signed arm restores TX.
    /// - Not armed → no-op (`vec![]`). A heartbeat can never resurrect/create an
    ///   arm.
    /// - `arm_jti != current arm's jti` → **rejected**: returns a
    ///   [`ArmEffect::HeartbeatRejected`] + audit; the window is NOT slid.
    /// - `seq <= last-accepted seq` (non-monotonic / replay) → **rejected** the
    ///   same way; the window is NOT slid.
    /// - Otherwise **accepted**: records `seq` as the new high-water mark and
    ///   slides the dead-man window to `now`. Returns `vec![]` (heartbeats
    ///   were never audited on the happy path — kept quiet to avoid log spam).
    ///
    /// Only an unexpired arm can accept a heartbeat: one arriving at or after
    /// the deadline is too late, however valid its `armJti` and `seq`.
    pub fn heartbeat(&mut self, arm_jti: &str, seq: u64, now: ArmTime) -> Vec<ArmEffect> {
        let expired = self.tick(now);
        if !expired.is_empty() {
            return expired;
        }
        let now_ms = now.unix_ms;
        let s = match self.session.as_mut() {
            Some(s) => s,
            // Not armed: a heartbeat can never create an arm. Silent no-op.
            None => return Vec::new(),
        };

        // Bind to THIS arm: a heartbeat naming a different (or stale) arm must
        // not slide the live arm's window.
        if arm_jti != s.current_jti {
            let reason = "arm_jti mismatch";
            return vec![
                ArmEffect::Audit(AuditEvent {
                    ts_unix_ms: now_ms,
                    kind: AuditKind::TxDenied,
                    operator_callsign: Some(s.operator_callsign.clone()),
                    detail: format!("heartbeat rejected: {reason}"),
                }),
                ArmEffect::HeartbeatRejected { reason },
            ];
        }

        // Monotonic seq: reject a replayed or out-of-order heartbeat. This is
        // THE guard that stops a replayed heartbeat from holding the arm open.
        if let Some(last) = s.last_heartbeat_seq {
            if seq <= last {
                let reason = "non-monotonic seq";
                return vec![
                    ArmEffect::Audit(AuditEvent {
                        ts_unix_ms: now_ms,
                        kind: AuditKind::TxDenied,
                        operator_callsign: Some(s.operator_callsign.clone()),
                        detail: format!("heartbeat rejected: {reason} (seq={seq}, last={last})"),
                    }),
                    ArmEffect::HeartbeatRejected { reason },
                ];
            }
        }

        // Accept: advance the high-water seq and slide the dead-man window.
        s.last_heartbeat_seq = Some(seq);
        s.last_heartbeat = now.monotonic;
        Vec::new()
    }

    /// Explicit operator/coordinator disarm. No-op (empty effects) if not armed.
    pub fn disarm(&mut self, now_ms: i64) -> Vec<ArmEffect> {
        self.disarm_with_reason(DisarmReason::OperatorDisarm, now_ms)
    }

    /// Disarm with an explicit attribution reason. This is used by fail-safe
    /// component teardown so the audit record does not misattribute a crash to
    /// an operator action. No-op (empty effects) if not armed.
    pub fn disarm_with_reason(&mut self, reason: DisarmReason, now_ms: i64) -> Vec<ArmEffect> {
        match self.session.take() {
            Some(s) => vec![
                ArmEffect::Audit(AuditEvent {
                    ts_unix_ms: now_ms,
                    kind: AuditKind::Disarmed,
                    operator_callsign: Some(s.operator_callsign),
                    detail: format!("disarmed: {}", reason.as_str()),
                }),
                ArmEffect::Disarmed { reason },
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
    /// record with the reason. TTL is checked first if both fire. Expiry is
    /// terminal; repeated calls after it are no-ops.
    pub fn tick(&mut self, now: ArmTime) -> Vec<ArmEffect> {
        let reason = self.session.as_ref().and_then(|s| s.expired_reason(now));

        match reason {
            Some(reason) => {
                let s = self.session.take().expect("armed by construction");
                vec![
                    ArmEffect::Audit(AuditEvent {
                        ts_unix_ms: now.unix_ms,
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
    /// currently armed AND wall `now < armedUntil` AND monotonic time since the
    /// arm `< lifetime` AND monotonic silence since the last heartbeat
    /// `< HEARTBEAT_TIMEOUT_MS` AND `local_consent == true` AND
    /// `local_kill == false` AND the grant carries TX scope.
    ///
    /// Pure read — never mutates, never resurrects an expired arm, and
    /// independently re-checks the clock so it is correct even if `tick` has not
    /// run for this `now`.
    pub fn tx_permitted(&self, now: ArmTime) -> bool {
        matches!(self.tx_permit_reason(now), TxPermit::Permitted)
    }

    /// The same gate as [`tx_permitted`](Self::tx_permitted), but returning the
    /// first-closed gate for audit `detail` / diagnostics. Evaluation order:
    /// NotArmed → NoTxScope → Expired → HeartbeatLost → NoLocalConsent →
    /// LocallyKilled. The *result* is an AND; the order only picks which reason
    /// is reported when several gates are closed.
    pub fn tx_permit_reason(&self, now: ArmTime) -> TxPermit {
        let s = match self.session.as_ref() {
            Some(s) => s,
            None => return TxPermit::Denied(DenyReason::NotArmed),
        };

        // Defense-in-depth: an armed session is only ever created with
        // scope_tx == true, but check anyway so the invariant holds structurally.
        if !s.scope_tx {
            return TxPermit::Denied(DenyReason::NoTxScope);
        }
        match s.expired_reason(now) {
            Some(DisarmReason::TtlExpired) => return TxPermit::Denied(DenyReason::Expired),
            Some(_) => return TxPermit::Denied(DenyReason::HeartbeatLost),
            None => {}
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

    const JTI: &str = "arm-jti-1";
    const CLIENT_KEY_ID: &str = "clientKeyId000000";

    /// One shared monotonic origin for every test timeline.
    fn mono_base() -> Instant {
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now)
    }

    /// Both clocks at `ms`: wall = `ms` unix ms, monotonic = origin + `ms`.
    fn at(ms: i64) -> ArmTime {
        at2(ms, ms)
    }

    /// Independent clocks: wall `unix_ms`, monotonic origin + `mono_ms`.
    fn at2(unix_ms: i64, mono_ms: i64) -> ArmTime {
        ArmTime {
            unix_ms,
            monotonic: mono_base() + Duration::from_millis(mono_ms.unsigned_abs()),
        }
    }

    fn grant(scope_tx: bool) -> VerifiedArmGrant {
        VerifiedArmGrant {
            operator_callsign: "N0CALL".to_string(),
            ttl_ms: TTL,
            armed_until_ms: T0 + TTL,
            heartbeat_interval_sec: 10,
            scope_tx,
            jti: JTI.to_string(),
            client_key_id: CLIENT_KEY_ID.to_string(),
        }
    }

    /// Assert an effect list contains a `HeartbeatRejected`.
    fn is_hb_rejected(effects: &[ArmEffect]) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, ArmEffect::HeartbeatRejected { .. }))
    }

    /// A helper: armed at T0 with TX scope + consent ON, no kill.
    fn armed_consented() -> ArmState {
        let mut st = ArmState::new();
        st.arm(grant(true), at(T0));
        st.set_local_consent(true, T0);
        st
    }

    // --- baseline ----------------------------------------------------------

    #[test]
    fn fresh_state_is_not_permitted_not_armed() {
        let st = ArmState::new();
        assert!(!st.tx_permitted(at(T0)));
        assert_eq!(
            st.tx_permit_reason(at(T0)),
            TxPermit::Denied(DenyReason::NotArmed)
        );
        assert!(!st.is_armed());
        assert!(!st.local_consent());
        assert!(!st.local_kill());
    }

    #[test]
    fn armed_with_consent_and_fresh_heartbeat_is_permitted() {
        let st = armed_consented();
        assert!(st.tx_permitted(at(T0)));
        assert_eq!(st.tx_permit_reason(at(T0)), TxPermit::Permitted);
        assert_eq!(st.operator_callsign(), Some("N0CALL"));
    }

    /// CQD-11 / dispensa Q-0052: `armed_client_key_id()` exposes the
    /// CRYPTOGRAPHIC client identity the live arm was verified for (distinct
    /// from `operator_callsign()`, the ham-radio attribution) — `None` when
    /// not armed, `Some(client_key_id)` while armed, and back to `None` once
    /// disarmed. This is what the Q-0043 authorization-poll task reads to
    /// detect that a live cqdx refresh has revoked the current controller's
    /// TX-arm eligibility.
    #[test]
    fn armed_client_key_id_tracks_the_live_arm() {
        let mut st = ArmState::new();
        assert_eq!(st.armed_client_key_id(), None, "not armed yet");
        st.arm(grant(true), at(T0));
        assert_eq!(st.armed_client_key_id(), Some(CLIENT_KEY_ID));
        st.disarm(T0 + 1_000);
        assert_eq!(st.armed_client_key_id(), None, "cleared after disarm");
    }

    #[test]
    fn arm_without_consent_is_never_permitted() {
        let mut st = ArmState::new();
        st.arm(grant(true), at(T0));
        // consent still default OFF
        assert!(!st.tx_permitted(at(T0)));
        assert_eq!(
            st.tx_permit_reason(at(T0)),
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
            st.tx_permitted(at(just_before)),
            "permitted 1ms before deadline"
        );
        assert!(
            !st.tx_permitted(at(at_deadline)),
            "denied exactly at deadline (>=)"
        );
        assert_eq!(
            st.tx_permit_reason(at(at_deadline)),
            TxPermit::Denied(DenyReason::HeartbeatLost)
        );
    }

    #[test]
    fn valid_monotonic_heartbeats_each_slide_the_window() {
        let mut st = armed_consented();
        // seq 1, 2, 3 each accepted (empty effects) and each slides the window.
        for (i, seq) in [1u64, 2, 3].into_iter().enumerate() {
            let hb = T0 + (i as i64 + 1) * 5_000;
            let effects = st.heartbeat(JTI, seq, at(hb));
            assert!(effects.is_empty(), "accepted heartbeat emits no effects");
            // Window now measured from `hb`: still permitted just before deadline.
            assert!(st.tx_permitted(at(hb + HEARTBEAT_TIMEOUT_MS - 1)));
        }
        // After the last accepted heartbeat (at T0+15_000) TX stays permitted
        // across the whole interval up to its own deadline.
        let last = T0 + 15_000;
        assert!(st.tx_permitted(at(last + HEARTBEAT_TIMEOUT_MS - 1)));
        assert!(!st.tx_permitted(at(last + HEARTBEAT_TIMEOUT_MS)));
    }

    /// THE finding: a **replayed** heartbeat must NOT hold the arm open past its
    /// dead-man window. seq 5 is accepted; a later replay of seq 5 (and seq 3) is
    /// rejected and the window does NOT slide, so `tx_permitted` flips false at
    /// the ORIGINAL `seq-5-time + HEARTBEAT_TIMEOUT_MS` even though the replay
    /// arrived after it.
    #[test]
    fn replayed_heartbeat_does_not_slide_the_deadman_window() {
        let mut st = armed_consented();
        let accepted_at = T0 + 10_000;
        assert!(st.heartbeat(JTI, 5, at(accepted_at)).is_empty());
        let deadline = accepted_at + HEARTBEAT_TIMEOUT_MS;
        // Replay seq 5 LATER (just before the deadline) — must be rejected and
        // must NOT slide the window.
        let replay_at = deadline - 1;
        let e1 = st.heartbeat(JTI, 5, at(replay_at));
        assert!(is_hb_rejected(&e1), "replayed seq must be rejected");
        // An even-lower seq is likewise rejected.
        let e2 = st.heartbeat(JTI, 3, at(replay_at));
        assert!(is_hb_rejected(&e2), "lower seq must be rejected");
        // Because neither replay slid the window, TX is denied at the ORIGINAL
        // deadline — the dead-man still expires on schedule.
        assert!(
            st.tx_permitted(at(deadline - 1)),
            "permitted 1ms before deadline"
        );
        assert!(
            !st.tx_permitted(at(deadline)),
            "dead-man expires on schedule; a replay cannot hold the arm open"
        );
        assert_eq!(
            st.tx_permit_reason(at(deadline)),
            TxPermit::Denied(DenyReason::HeartbeatLost)
        );
    }

    #[test]
    fn heartbeat_with_wrong_arm_jti_is_rejected_and_window_unchanged() {
        let mut st = armed_consented();
        let hb = T0 + 10_000;
        let effects = st.heartbeat("some-other-arm", 1, at(hb));
        assert!(is_hb_rejected(&effects), "wrong arm_jti must be rejected");
        // The window was never seeded past T0: original dead-man deadline holds.
        assert!(st.tx_permitted(at(T0 + HEARTBEAT_TIMEOUT_MS - 1)));
        assert!(!st.tx_permitted(at(T0 + HEARTBEAT_TIMEOUT_MS)));
    }

    #[test]
    fn heartbeat_while_not_armed_is_a_noop() {
        let mut st = ArmState::new();
        st.set_local_consent(true, T0);
        // Must not arm, must not panic, returns empty effects.
        let effects = st.heartbeat(JTI, 1, at(T0));
        assert!(effects.is_empty());
        assert!(!st.is_armed());
        assert!(!st.tx_permitted(at(T0)));
        assert_eq!(
            st.tx_permit_reason(at(T0)),
            TxPermit::Denied(DenyReason::NotArmed)
        );
    }

    /// A fresh arm (new grant, new jti) resets the seq high-water mark: a low seq
    /// is accepted for the NEW arm even though the OLD arm reached a high seq.
    #[test]
    fn fresh_arm_resets_the_heartbeat_seq() {
        let mut st = ArmState::new();
        st.set_local_consent(true, T0);
        st.arm(grant(true), at(T0)); // jti = JTI
        assert!(st.heartbeat(JTI, 9, at(T0 + 1_000)).is_empty());
        // Re-arm with a DIFFERENT jti (fresh grant): seq resets.
        let g2 = VerifiedArmGrant {
            operator_callsign: "N0CALL".to_string(),
            ttl_ms: TTL,
            armed_until_ms: T0 + TTL,
            heartbeat_interval_sec: 10,
            scope_tx: true,
            jti: "arm-jti-2".to_string(),
            client_key_id: CLIENT_KEY_ID.to_string(),
        };
        st.arm(g2, at(T0 + 2_000));
        // A low seq (1) is accepted for the NEW arm despite the old arm's seq 9.
        let effects = st.heartbeat("arm-jti-2", 1, at(T0 + 3_000));
        assert!(effects.is_empty(), "new arm accepts a low seq");
        assert!(st.tx_permitted(at(T0 + 3_000 + HEARTBEAT_TIMEOUT_MS - 1)));
        // And the OLD arm's jti is now rejected (it names a defunct arm).
        assert!(is_hb_rejected(&st.heartbeat(JTI, 10, at(T0 + 3_000))));
    }

    // --- PAN-97: a lapsed arm needs a fresh signed arm ---------------------

    fn grant_full(ttl_ms: i64, armed_until_ms: i64, cadence: i64, jti: &str) -> VerifiedArmGrant {
        VerifiedArmGrant {
            operator_callsign: "N0CALL".to_string(),
            ttl_ms,
            armed_until_ms,
            heartbeat_interval_sec: cadence,
            scope_tx: true,
            jti: jti.to_string(),
            client_key_id: CLIENT_KEY_ID.to_string(),
        }
    }

    /// Count (`Disarmed` audits, `Disarmed { reason }` effects) in `effects`.
    fn disarm_counts(effects: &[ArmEffect], reason: DisarmReason) -> (usize, usize) {
        let audits = effects
            .iter()
            .filter(|e| matches!(e, ArmEffect::Audit(ev) if ev.kind == AuditKind::Disarmed))
            .count();
        let disarms = effects
            .iter()
            .filter(|e| matches!(e, ArmEffect::Disarmed { reason: r } if *r == reason))
            .count();
        (audits, disarms)
    }

    #[test]
    fn late_heartbeat_at_or_after_deadline_expires_instead_of_restoring_tx() {
        for late in [T0 + HEARTBEAT_TIMEOUT_MS, T0 + HEARTBEAT_TIMEOUT_MS + 1] {
            let mut st = armed_consented();
            assert!(!st.tx_permitted(at(late)));
            // Correct jti, fresh seq, no tick beforehand.
            let effects = st.heartbeat(JTI, 1, at(late));
            assert_eq!(
                effects.len(),
                2,
                "exactly one audit + one disarm: {effects:?}"
            );
            assert_eq!(disarm_counts(&effects, DisarmReason::HeartbeatLost), (1, 1));
            assert!(matches!(&effects[0], ArmEffect::Audit(ev)
                if ev.detail == "auto-disarm: heartbeat-lost" && ev.ts_unix_ms == late));
            assert!(!st.is_armed(), "late heartbeat must remove the arm");
            assert!(!st.tx_permitted(at(late)));
            assert_eq!(
                st.tx_permit_reason(at(late)),
                TxPermit::Denied(DenyReason::NotArmed)
            );
        }
    }

    #[test]
    fn heartbeat_just_before_deadline_refreshes_and_exact_deadline_expires() {
        let mut st = armed_consented();
        let hb1 = T0 + HEARTBEAT_TIMEOUT_MS - 1;
        assert!(
            st.heartbeat(JTI, 1, at(hb1)).is_empty(),
            "1ms early is accepted"
        );
        assert!(st.tx_permitted(at(hb1 + HEARTBEAT_TIMEOUT_MS - 1)));
        let effects = st.heartbeat(JTI, 2, at(hb1 + HEARTBEAT_TIMEOUT_MS));
        assert_eq!(disarm_counts(&effects, DisarmReason::HeartbeatLost), (1, 1));
        assert!(!st.is_armed());
    }

    #[test]
    fn late_heartbeat_after_several_accepted_seqs_expires() {
        let mut st = armed_consented();
        for seq in 1..=4u64 {
            assert!(st
                .heartbeat(JTI, seq, at(T0 + seq as i64 * 10_000))
                .is_empty());
        }
        let late = T0 + 40_000 + HEARTBEAT_TIMEOUT_MS + 5;
        let effects = st.heartbeat(JTI, 5, at(late));
        assert_eq!(disarm_counts(&effects, DisarmReason::HeartbeatLost), (1, 1));
        assert!(!st.tx_permitted(at(late)));
    }

    #[test]
    fn late_wrong_jti_or_replayed_heartbeat_expires_before_any_rejection() {
        let late = T0 + HEARTBEAT_TIMEOUT_MS + 1;
        let mut st = armed_consented();
        let effects = st.heartbeat("some-other-arm", 1, at(late));
        assert!(
            !is_hb_rejected(&effects),
            "expiry runs before the jti check"
        );
        assert_eq!(disarm_counts(&effects, DisarmReason::HeartbeatLost), (1, 1));
        assert!(!st.is_armed());

        let mut st = armed_consented();
        assert!(st.heartbeat(JTI, 3, at(T0 + 1_000)).is_empty());
        let effects = st.heartbeat(JTI, 3, at(T0 + 1_000 + HEARTBEAT_TIMEOUT_MS));
        assert!(
            !is_hb_rejected(&effects),
            "expiry runs before the seq check"
        );
        assert_eq!(disarm_counts(&effects, DisarmReason::HeartbeatLost), (1, 1));
        assert!(!st.is_armed());
    }

    #[test]
    fn expiry_is_terminal_and_never_duplicated() {
        let mut st = armed_consented();
        let late = T0 + HEARTBEAT_TIMEOUT_MS;
        assert_eq!(st.tick(at(late)).len(), 2);
        for (k, seq) in [2u64, 3, 4].into_iter().enumerate() {
            let t = at(late + k as i64);
            assert!(st.heartbeat(JTI, seq, t).is_empty(), "no duplicate disarm");
            assert!(st.tick(t).is_empty(), "no duplicate disarm");
            assert!(!st.is_armed(), "no recreated session");
            assert!(!st.tx_permitted(t));
        }
    }

    #[test]
    fn ttl_expiry_before_a_heartbeat_removes_the_session_with_ttl_reason() {
        // TTL alone.
        let mut st = ArmState::new();
        st.set_local_consent(true, T0);
        st.arm(grant_full(10_000, T0 + 10_000, 10, JTI), at(T0));
        assert!(st.heartbeat(JTI, 1, at(T0 + 5_000)).is_empty());
        let effects = st.heartbeat(JTI, 2, at(T0 + 10_000));
        assert_eq!(disarm_counts(&effects, DisarmReason::TtlExpired), (1, 1));
        assert!(!st.is_armed());

        // TTL and dead-man both passed: TTL is the reason, via heartbeat too.
        let mut st = ArmState::new();
        st.set_local_consent(true, T0);
        st.arm(grant_full(5_000, T0 + 5_000, 10, JTI), at(T0));
        let effects = st.heartbeat(JTI, 1, at(T0 + 40_000));
        assert_eq!(disarm_counts(&effects, DisarmReason::TtlExpired), (1, 1));
        assert!(matches!(&effects[0], ArmEffect::Audit(ev)
            if ev.detail == "auto-disarm: ttl-expired"));
    }

    #[test]
    fn fresh_signed_arm_after_loss_restores_tx_and_old_jti_cannot_refresh_it() {
        let mut st = armed_consented();
        let late = T0 + HEARTBEAT_TIMEOUT_MS + 1;
        st.heartbeat(JTI, 1, at(late));
        assert!(!st.tx_permitted(at(late)));

        st.arm(grant_full(TTL, late + TTL, 10, "arm-jti-2"), at(late));
        assert!(st.tx_permitted(at(late)), "a fresh signed arm restores TX");
        assert!(st.heartbeat("arm-jti-2", 1, at(late + 10_000)).is_empty());
        // The old arm's jti is rejected and does not slide the new window.
        assert!(is_hb_rejected(&st.heartbeat(JTI, 9, at(late + 20_000))));
        assert!(st.tx_permitted(at(late + 10_000 + HEARTBEAT_TIMEOUT_MS - 1)));
        assert!(!st.tx_permitted(at(late + 10_000 + HEARTBEAT_TIMEOUT_MS)));
    }

    // --- PAN-97: wall vs monotonic time -----------------------------------

    #[test]
    fn wall_clock_rollback_cannot_extend_the_heartbeat_window() {
        let mut st = armed_consented();
        // Wall steps back 20 s while monotonic time passes the dead-man limit.
        let now = at2(T0 - 20_000, T0 + HEARTBEAT_TIMEOUT_MS);
        assert_eq!(
            st.tx_permit_reason(now),
            TxPermit::Denied(DenyReason::HeartbeatLost)
        );
        let effects = st.heartbeat(JTI, 1, now);
        assert_eq!(disarm_counts(&effects, DisarmReason::HeartbeatLost), (1, 1));
        assert!(!st.is_armed());
    }

    #[test]
    fn wall_clock_forward_step_short_of_armed_until_leaves_liveness_fresh() {
        let st = armed_consented();
        // Wall jumps 100 s forward (still before armedUntil = T0 + TTL); the
        // monotonic heartbeat age is only 1 s.
        assert!(st.tx_permitted(at2(T0 + 100_000, T0 + 1_000)));
    }

    #[test]
    fn signed_armed_until_expires_on_wall_time_despite_fresh_heartbeats() {
        let mut st = armed_consented();
        // Wall reaches armedUntil (T0 + TTL) while monotonic age is 1 s.
        let now = at2(T0 + TTL, T0 + 1_000);
        assert_eq!(
            st.tx_permit_reason(now),
            TxPermit::Denied(DenyReason::Expired)
        );
        let effects = st.heartbeat(JTI, 1, now);
        assert_eq!(disarm_counts(&effects, DisarmReason::TtlExpired), (1, 1));

        let mut st = armed_consented();
        assert_eq!(
            disarm_counts(&st.tick(now), DisarmReason::TtlExpired),
            (1, 1),
            "the sweep honours the signed deadline too"
        );
    }

    #[test]
    fn monotonic_lifetime_expires_even_when_wall_rolls_back() {
        let mut st = armed_consented();
        // Timely heartbeats while wall time is rolled back 60 s.
        let mut seq = 0;
        for mono in (T0 + 10_000..T0 + TTL).step_by(10_000) {
            seq += 1;
            assert!(st.heartbeat(JTI, seq, at2(mono - 60_000, mono)).is_empty());
        }
        let now = at2(T0 + TTL - 60_000, T0 + TTL);
        assert_eq!(
            st.tx_permit_reason(now),
            TxPermit::Denied(DenyReason::Expired)
        );
        assert_eq!(
            disarm_counts(&st.heartbeat(JTI, seq + 1, now), DisarmReason::TtlExpired),
            (1, 1)
        );
    }

    #[test]
    fn delayed_acceptance_cannot_extend_the_signed_grant() {
        // Verified at T0 for 60 s (armedUntil = T0 + 60 s), accepted 10 s later.
        let mut st = ArmState::new();
        st.set_local_consent(true, T0);
        let effects = st.arm(grant_full(60_000, T0 + 60_000, 10, JTI), at(T0 + 10_000));
        assert!(matches!(&effects[0], ArmEffect::Audit(ev)
            if ev.detail.starts_with("armed ttl_ms=50000 ")));
        st.heartbeat(JTI, 1, at(T0 + 30_000));
        st.heartbeat(JTI, 2, at(T0 + 50_000));
        // Wall rolled back so only the monotonic lifetime (50 s) can expire it.
        assert!(st.tx_permitted(at2(T0, T0 + 59_999)));
        assert!(!st.tx_permitted(at2(T0, T0 + 60_000)));

        // A backward wall step between verification and acceptance must not
        // enlarge the verified cap: remaining = 90 s, cap stays 60 s.
        let mut st = ArmState::new();
        st.set_local_consent(true, T0);
        st.arm(
            grant_full(60_000, T0 + 60_000, 10, JTI),
            at2(T0 - 30_000, T0),
        );
        st.heartbeat(JTI, 1, at2(T0 - 5_000, T0 + 25_000));
        st.heartbeat(JTI, 2, at2(T0 + 20_000, T0 + 50_000));
        assert!(st.tx_permitted(at2(T0 + 29_999, T0 + 59_999)));
        assert!(!st.tx_permitted(at2(T0 + 29_999, T0 + 60_000)));
    }

    #[test]
    fn already_expired_grant_cannot_arm() {
        let mut st = ArmState::new();
        st.set_local_consent(true, T0);
        for accept in [T0 + 60_000, T0 + 60_001] {
            let effects = st.arm(grant_full(60_000, T0 + 60_000, 10, JTI), at(accept));
            assert!(!st.is_armed());
            assert!(matches!(&effects[..], [ArmEffect::Audit(ev)]
                if ev.kind == AuditKind::TxDenied && ev.detail == "arm rejected: ttl-expired"));
        }
    }

    #[test]
    fn monotonic_sample_before_a_stored_instant_denies_and_cannot_refresh() {
        let mut st = ArmState::new();
        st.set_local_consent(true, T0);
        st.arm(grant(true), at2(T0, 10_000));
        // Earlier than the arm instant: the lifetime cannot be measured.
        assert_eq!(
            st.tx_permit_reason(at2(T0, 5_000)),
            TxPermit::Denied(DenyReason::Expired)
        );
        assert!(st.heartbeat(JTI, 1, at2(T0, 20_000)).is_empty());
        // After the arm but earlier than the last heartbeat.
        let back = at2(T0, 15_000);
        assert_eq!(
            st.tx_permit_reason(back),
            TxPermit::Denied(DenyReason::HeartbeatLost)
        );
        let effects = st.heartbeat(JTI, 2, back);
        assert_eq!(disarm_counts(&effects, DisarmReason::HeartbeatLost), (1, 1));
        assert!(!st.is_armed());
    }

    #[test]
    fn audit_timestamps_use_wall_time_not_monotonic_offsets() {
        let mut st = ArmState::new();
        let effects = st.arm(grant(true), at2(T0, 7));
        assert!(matches!(&effects[0], ArmEffect::Audit(ev) if ev.ts_unix_ms == T0));
        let effects = st.tick(at2(T0 + 1, 7 + HEARTBEAT_TIMEOUT_MS));
        assert!(matches!(&effects[0], ArmEffect::Audit(ev) if ev.ts_unix_ms == T0 + 1));
    }

    #[test]
    fn every_accepted_cadence_keeps_the_fixed_30s_silence_limit() {
        for cadence in [5i64, 10, 15] {
            let mut st = ArmState::new();
            st.set_local_consent(true, T0);
            let effects = st.arm(grant_full(TTL, T0 + TTL, cadence, JTI), at(T0));
            let want = format!(
                "armed ttl_ms={TTL} heartbeat_interval_sec={cadence} heartbeat_timeout_ms=30000"
            );
            assert!(matches!(&effects[0], ArmEffect::Audit(ev) if ev.detail == want));
            // Scheduled heartbeats at the cadence keep the arm alive.
            let step = cadence * 1_000;
            let mut last = T0;
            for seq in 1..=(60_000 / step) as u64 {
                last = T0 + seq as i64 * step;
                assert!(st.heartbeat(JTI, seq, at(last)).is_empty());
                assert!(st.tx_permitted(at(last)));
            }
            // Silence expires at the same boundary for every cadence.
            assert!(st.tx_permitted(at(last + HEARTBEAT_TIMEOUT_MS - 1)));
            assert!(!st.tx_permitted(at(last + HEARTBEAT_TIMEOUT_MS)));
            assert_eq!(
                disarm_counts(
                    &st.tick(at(last + HEARTBEAT_TIMEOUT_MS)),
                    DisarmReason::HeartbeatLost
                ),
                (1, 1)
            );
        }
    }

    // --- TTL boundary ------------------------------------------------------

    #[test]
    fn ttl_boundary_is_exact() {
        // Use a short TTL well within the heartbeat window so TTL is the gate.
        let mut st = ArmState::new();
        let short_ttl = 10_000;
        st.arm(
            VerifiedArmGrant {
                operator_callsign: "N0CALL".into(),
                ttl_ms: short_ttl,
                armed_until_ms: T0 + short_ttl,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: JTI.to_string(),
                client_key_id: CLIENT_KEY_ID.to_string(),
            },
            at(T0),
        );
        st.set_local_consent(true, T0);
        assert!(
            st.tx_permitted(at(T0 + short_ttl - 1)),
            "permitted 1ms before ttl"
        );
        assert!(
            !st.tx_permitted(at(T0 + short_ttl)),
            "denied exactly at ttl (>=)"
        );
        assert_eq!(
            st.tx_permit_reason(at(T0 + short_ttl)),
            TxPermit::Denied(DenyReason::Expired)
        );
    }

    // --- local kill --------------------------------------------------------

    #[test]
    fn local_kill_denies_immediately_and_survives_heartbeats() {
        let mut st = armed_consented();
        assert!(st.tx_permitted(at(T0)));
        st.set_local_kill(true, T0);
        assert!(!st.tx_permitted(at(T0)), "killed => denied immediately");
        assert_eq!(
            st.tx_permit_reason(at(T0)),
            TxPermit::Denied(DenyReason::LocallyKilled)
        );
        // Fresh heartbeats cannot un-kill.
        st.heartbeat(JTI, 1, at(T0 + 1_000));
        st.heartbeat(JTI, 2, at(T0 + 2_000));
        assert!(!st.tx_permitted(at(T0 + 2_000)));
        // Only clearing the kill restores permission (still armed + consented).
        st.set_local_kill(false, T0 + 3_000);
        assert!(st.tx_permitted(at(T0 + 3_000)));
    }

    #[test]
    fn kill_engaged_before_arm_blocks_a_later_valid_arm() {
        let mut st = ArmState::new();
        st.set_local_kill(true, T0);
        st.arm(grant(true), at(T0));
        st.set_local_consent(true, T0);
        assert!(!st.tx_permitted(at(T0)));
        assert_eq!(
            st.tx_permit_reason(at(T0)),
            TxPermit::Denied(DenyReason::LocallyKilled)
        );
    }

    // --- local consent -----------------------------------------------------

    #[test]
    fn revoking_consent_denies_immediately() {
        let mut st = armed_consented();
        assert!(st.tx_permitted(at(T0)));
        st.set_local_consent(false, T0);
        assert!(!st.tx_permitted(at(T0)));
        assert_eq!(
            st.tx_permit_reason(at(T0)),
            TxPermit::Denied(DenyReason::NoLocalConsent)
        );
    }

    // --- scope -------------------------------------------------------------

    #[test]
    fn grant_without_tx_scope_is_rejected_and_never_permits() {
        let mut st = ArmState::new();
        let effects = st.arm(grant(false), at(T0));
        st.set_local_consent(true, T0);
        assert!(!st.is_armed(), "no-scope grant does not arm");
        assert!(!st.tx_permitted(at(T0)));
        assert_eq!(
            st.tx_permit_reason(at(T0)),
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
                operator_callsign: "N0CALL".into(),
                ttl_ms: short_ttl,
                armed_until_ms: T0 + short_ttl,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: JTI.to_string(),
                client_key_id: CLIENT_KEY_ID.to_string(),
            },
            at(T0),
        );
        st.set_local_consent(true, T0);
        let after_expiry = T0 + short_ttl + 5;
        assert!(!st.tx_permitted(at(after_expiry)));
        assert_eq!(
            st.tx_permit_reason(at(after_expiry)),
            TxPermit::Denied(DenyReason::Expired)
        );
        // A heartbeat after expiry must NOT re-permit — it expires the arm
        // first (TTL reason) and is then ignored.
        let effects = st.heartbeat(JTI, 1, at(after_expiry));
        assert!(effects.contains(&ArmEffect::Disarmed {
            reason: DisarmReason::TtlExpired
        }));
        assert!(!st.is_armed());
        assert!(!st.tx_permitted(at(after_expiry)));
        // Only a fresh arm restores permission.
        st.arm(
            VerifiedArmGrant {
                operator_callsign: "N0CALL".into(),
                ttl_ms: short_ttl,
                armed_until_ms: after_expiry + short_ttl,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: JTI.to_string(),
                client_key_id: CLIENT_KEY_ID.to_string(),
            },
            at(after_expiry),
        );
        assert!(st.tx_permitted(at(after_expiry)));
    }

    #[test]
    fn heartbeat_lost_then_tick_disarms_and_a_later_heartbeat_does_not_resurrect() {
        let mut st = armed_consented();
        let dead = T0 + HEARTBEAT_TIMEOUT_MS;
        // tick past the dead-man window disarms.
        let effects = st.tick(at(dead));
        assert!(!st.is_armed());
        assert!(effects
            .iter()
            .any(|e| matches!(e, ArmEffect::Disarmed { reason } if *reason == DisarmReason::HeartbeatLost)));
        // A heartbeat now targets no session; still not permitted.
        assert!(st.heartbeat(JTI, 1, at(dead)).is_empty());
        assert!(!st.tx_permitted(at(dead)));
        assert_eq!(
            st.tx_permit_reason(at(dead)),
            TxPermit::Denied(DenyReason::NotArmed)
        );
    }

    // --- tick effects ------------------------------------------------------

    #[test]
    fn tick_before_any_deadline_is_a_noop() {
        let mut st = armed_consented();
        assert!(st.tick(at(T0 + 5_000)).is_empty());
        assert!(st.is_armed());
    }

    #[test]
    fn tick_past_ttl_emits_disarmed_and_audit_with_ttl_reason() {
        let mut st = ArmState::new();
        st.arm(
            VerifiedArmGrant {
                operator_callsign: "N0CALL".into(),
                ttl_ms: 10_000,
                armed_until_ms: T0 + 10_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: JTI.to_string(),
                client_key_id: CLIENT_KEY_ID.to_string(),
            },
            at(T0),
        );
        st.set_local_consent(true, T0);
        let effects = st.tick(at(T0 + 10_000));
        assert!(!st.is_armed());
        assert_eq!(effects.len(), 2);
        match &effects[0] {
            ArmEffect::Audit(ev) => {
                assert_eq!(ev.kind, AuditKind::Disarmed);
                assert!(ev.detail.contains("ttl-expired"));
                assert_eq!(ev.operator_callsign.as_deref(), Some("N0CALL"));
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
        let effects = st.tick(at(T0 + HEARTBEAT_TIMEOUT_MS));
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
                operator_callsign: "N0CALL".into(),
                ttl_ms: 5_000,
                armed_until_ms: T0 + 5_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: JTI.to_string(),
                client_key_id: CLIENT_KEY_ID.to_string(),
            },
            at(T0),
        );
        st.set_local_consent(true, T0);
        let effects = st.tick(at(T0 + 40_000)); // past ttl AND past heartbeat window
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
        assert!(!st.tx_permitted(at(T0 + 1_000)));
        // Disarm again = no effects.
        assert!(st.disarm(T0 + 2_000).is_empty());
    }

    #[test]
    fn disarm_with_component_crash_reason_is_truthfully_attributed() {
        assert_eq!(DisarmReason::ComponentCrash.as_str(), "component-crash");

        let mut st = armed_consented();
        let effects = st.disarm_with_reason(DisarmReason::ComponentCrash, T0 + 1_000);

        assert_eq!(effects.len(), 2);
        assert!(matches!(
            &effects[0],
            ArmEffect::Audit(event)
                if event.kind == AuditKind::Disarmed
                    && event.detail == "disarmed: component-crash"
        ));
        assert_eq!(
            effects[1],
            ArmEffect::Disarmed {
                reason: DisarmReason::ComponentCrash
            }
        );
        assert!(!st.is_armed());
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
    /// permitted ⇒ armed ∧ scope ∧ wall < armedUntil ∧ mono lifetime left ∧
    /// heartbeat-fresh (mono) ∧ consent ∧ ¬killed.
    ///
    /// Expiry is terminal in the model: once any deadline has passed, the next
    /// heartbeat or tick drops the arm, and a later heartbeat cannot revive it.
    #[derive(Clone)]
    struct Shadow {
        armed: bool,
        armed_at_mono: i64,
        lifetime: i64,
        armed_until_unix: i64,
        last_hb_mono: i64,
        scope_tx: bool,
        consent: bool,
        kill: bool,
    }

    impl Shadow {
        fn new() -> Self {
            Shadow {
                armed: false,
                armed_at_mono: 0,
                lifetime: 0,
                armed_until_unix: 0,
                last_hb_mono: 0,
                scope_tx: false,
                consent: false,
                kill: false,
            }
        }
        fn deadline_passed(&self, unix: i64, mono: i64) -> bool {
            unix >= self.armed_until_unix
                || mono < self.armed_at_mono
                || mono - self.armed_at_mono >= self.lifetime
                || mono < self.last_hb_mono
                || mono - self.last_hb_mono >= HEARTBEAT_TIMEOUT_MS
        }
        fn any_gate_closed(&self, unix: i64, mono: i64) -> bool {
            !self.armed
                || !self.scope_tx
                || self.deadline_passed(unix, mono)
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
            // Monotonic time only moves forward; wall time follows it with a
            // random skew (forward or backward steps) so the two clocks diverge.
            let mut mono: i64 = 1_000_000;
            let mut unix: i64 = 1_000_000;
            // Per-arm jti + a monotonic heartbeat seq so every heartbeat this
            // trajectory issues names the live arm with a fresh seq: only the
            // deadline decides whether it is accepted.
            let mut arm_ordinal: u64 = 0;
            let mut cur_jti = String::new();
            let mut hb_seq: u64 = 0;

            for _ in 0..40 {
                // Advance time by a bounded random amount (can cross deadlines)
                // with no tick in between.
                let dt = rng.below(40_000) as i64;
                mono += dt;
                unix += dt + rng.below(20_001) as i64 - 10_000;
                let now = at2(unix, mono);

                match rng.below(6) {
                    0 => {
                        // arm (random scope, random ttl in [1ms, 90s]); the
                        // signed deadline is `ttl` past wall time, as verified.
                        let scope = rng.below(2) == 1;
                        let ttl = 1 + rng.below(90_000) as i64;
                        arm_ordinal += 1;
                        let jti = format!("arm-{arm_ordinal}");
                        st.arm(
                            VerifiedArmGrant {
                                operator_callsign: "N0CALL".into(),
                                ttl_ms: ttl,
                                armed_until_ms: unix + ttl,
                                heartbeat_interval_sec: 10,
                                scope_tx: scope,
                                jti: jti.clone(),
                                client_key_id: CLIENT_KEY_ID.to_string(),
                            },
                            now,
                        );
                        // Shadow: arm only takes effect if scope_tx.
                        if scope {
                            sh.armed = true;
                            sh.armed_at_mono = mono;
                            sh.lifetime = ttl;
                            sh.armed_until_unix = unix + ttl;
                            sh.last_hb_mono = mono;
                            sh.scope_tx = true;
                            // A fresh arm resets the heartbeat sequence.
                            cur_jti = jti;
                            hb_seq = 0;
                        }
                        // (no-scope arm leaves prior session untouched, matching impl)
                    }
                    1 => {
                        hb_seq += 1;
                        st.heartbeat(&cur_jti, hb_seq, now);
                        if sh.armed {
                            if sh.deadline_passed(unix, mono) {
                                // Terminal: a late heartbeat expires the arm.
                                sh.armed = false;
                            } else {
                                sh.last_hb_mono = mono;
                            }
                        }
                    }
                    2 => {
                        st.disarm(unix);
                        sh.armed = false;
                    }
                    3 => {
                        let en = rng.below(2) == 1;
                        st.set_local_consent(en, unix);
                        sh.consent = en;
                    }
                    4 => {
                        let en = rng.below(2) == 1;
                        st.set_local_kill(en, unix);
                        sh.kill = en;
                    }
                    _ => {
                        st.tick(now);
                        if sh.armed && sh.deadline_passed(unix, mono) {
                            sh.armed = false;
                        }
                    }
                }

                // The stored arm and the model agree on terminal expiry.
                assert_eq!(st.is_armed(), sh.armed, "stored arm diverged from model");

                // Query at `now` and at a few future offsets (tx_permitted must be
                // correct even without a tick at that instant).
                for dt in [
                    0i64,
                    1,
                    HEARTBEAT_TIMEOUT_MS - 1,
                    HEARTBEAT_TIMEOUT_MS,
                    200_000,
                ] {
                    let (qu, qm) = (unix + dt, mono + dt);
                    let permitted = st.tx_permitted(at2(qu, qm));

                    // THE SAFETY INVARIANT: if permitted, no gate may be closed.
                    if permitted {
                        assert!(
                            !sh.any_gate_closed(qu, qm),
                            "SAFETY VIOLATION: tx_permitted(true) with a closed gate. \
                             shadow: armed={} scope={} armed_at_mono={} lifetime={} \
                             armed_until_unix={} last_hb_mono={} consent={} kill={} \
                             q=({qu},{qm})",
                            sh.armed,
                            sh.scope_tx,
                            sh.armed_at_mono,
                            sh.lifetime,
                            sh.armed_until_unix,
                            sh.last_hb_mono,
                            sh.consent,
                            sh.kill,
                        );
                    }
                    // And the converse: with every gate open, the impl permits.
                    assert_eq!(permitted, !sh.any_gate_closed(qu, qm));
                    // A Permitted verdict must agree the boolean is true.
                    let reason = st.tx_permit_reason(at2(qu, qm));
                    assert_eq!(permitted, matches!(reason, TxPermit::Permitted));
                }
            }
        }
    }
}
