//! Station-agent component — the authenticated remote-control transport.
//!
//! This component owns the **paired, Noise-encrypted control channel** to a
//! remote client (via the cqdx relay Durable Object) and is the ONLY place a
//! decrypted client control frame drives the coordinator. It is **default-OFF**
//! and **inert** unless the operator has both enabled it in config AND completed
//! pairing — a stock station is byte-identical to one built before this
//! component existed.
//!
//! ## What this component does (P3.4b + concurrent multi-client, 2026-07-20)
//!
//! - **Connect + authenticate + Noise handshake** to the relay, demuxing up to
//!   `multi_session::MAX_PEERS` (8 — the relay's own `MAX_CLIENTS` cap)
//!   CONCURRENT peers over that one socket, each with an INDEPENDENT Noise
//!   session ([`net::RealWsConn`] → [`MultiPeerSession`]). An unknown `src` is
//!   checked against the station-local `tx_allow_list` *before* any handshake
//!   state is created — an unlisted peer never costs a handshake, and a full
//!   peer map is dropped defensively (the relay's own 9th-client `CAPACITY`
//!   terminal code is the first line of defense).
//! - **Controller model — one controller at a time, free grab:** at most one
//!   established peer holds `controller` (arm/QSY/split/TX-initiation)
//!   at a time; every other connected peer is a live read-only viewer.
//!   `takeControl` from any admitted peer always succeeds (single-operator
//!   assumption) — if it displaces a different controller whose arm is live,
//!   that arm is **disarmed first** (audited); arms never transfer, the new
//!   controller must arm fresh through the full capabilityToken + clientSig
//!   grant. `releaseControl` from the current controller clears it (and
//!   disarms if armed). A control-mutating action from `controller == None`
//!   **implicitly grabs** control (rule kept for backward compatibility — a
//!   single legacy client that never sends `takeControl` arms exactly as it
//!   always has). *Precision note:* "byte-identical" here means a single
//!   connected client's **dispatch/arm/control** behavior is byte-for-byte
//!   what it was before this branch — NOT that every byte sent to the client is
//!   unchanged. Its INBOUND relay stream is now a strict *superset*: it also
//!   receives a `ControlState` greeting on session establishment plus any
//!   read-stream events, both carried by pre-existing rig-api.v1 types (no new
//!   wire event). A control-mutating action from a non-controller while
//!   someone else holds control is refused (`warn!` + audited + an error
//!   frame back to that peer), never an implicit grab. Two **deliberate
//!   safety asymmetries**: `Disarm` and `Heartbeat` are accepted from ANY
//!   established peer regardless of controller state (fail-safe TX-OFF beats
//!   exclusivity; a heartbeat's existing `arm_jti` + monotonic-`seq` binding
//!   already prevents anyone but the armer from sliding the dead-man window).
//!   Controller `down`, whole-session teardown, or component shutdown clears
//!   `controller` and disarms; a mere listener disconnecting disarms nothing.
//! - **Verify + arm** on a `txArmGrant`: the two-stage crown-jewel verification
//!   ([`CapabilityVerifier`]) mints a [`VerifiedArmGrant`] which is fed into the
//!   coordinator's shared `remote_tx_arm` [`ArmState`]. Once armed (AND the
//!   operator's local `remote_tx_enabled` consent is ON), a `TxOrigin::Remote`
//!   transmit request will key PTT at the TX worker's arm gate.
//! - **Heartbeat / disarm / control-loss** all drive the same `ArmState`:
//!   a heartbeat that names the current arm (`arm_jti`) with a monotonic `seq`
//!   slides the dead-man window (a replayed/wrong-arm one is rejected, window
//!   unchanged); an explicit `Disarm`, a peer
//!   `down` presence, a session teardown, or any terminal error **disarms**
//!   (fail TX-off on control-channel loss, Part-97).
//! - **Non-TX rig control** (`Qsy`, `SetSplit`) is forwarded onto the existing
//!   coordinator bus (`RigControlMessage`) — read/QSY/split only — and is
//!   gated behind the controller rules above.
//! - **TX-initiation** (`callStation` / `answerCaller` / `startCq`) is
//!   **audited but deferred to P3.4c** — v1 does NOT route these through the QSO
//!   engine; each is logged "not-yet-supported in v1" and does NOT key TX.
//! - **Read stream (now real, over the relay):** the station agent subscribes
//!   to the shared [`super::remote_gateway::DisplayFeed`] — the same
//!   bus-to-`ServerEvent` translation pump the localhost `remote_gateway`
//!   uses — and, between (timeout-bounded) control-frame reads, drains it and
//!   `broadcast()`s each event as `ServerFrame::Event` JSON inside per-peer
//!   encrypted `env` frames to every established peer. The feed is started
//!   when *either* the localhost gateway or the station agent is enabled
//!   (`display_feed_enabled`); the tokio `broadcast` ring is a drop-oldest
//!   queue (lossy by design — control frames are never queued behind it).
//!   Per-peer `ControlState` (`controlHeldByMe`, `transmitArmed`) is sent on
//!   every controller/arm transition and on session establishment — no new
//!   wire event was needed; rig-api.v1 already defined it.
//! - **Boot-gated, not hot-started:** the shared display feed is started ONCE
//!   at coordinator startup, from the config as it stands at that instant. If
//!   the station-agent component is turned on via a config *hot-reload* after
//!   boot (rather than at startup), its read stream stays inert until the next
//!   restart — the feed is never hot-started mid-run. This matches the
//!   station-agent component's own boot-only start gating and is not a new
//!   limitation this branch introduces; it was simply previously undocumented.
//!
//! ## Inert-when-off invariant
//!
//! Disabled OR unpaired OR missing relay/pairing URL → the component spawns a
//! no-op drain task (so additive bus sends never flood) and does nothing else.
//! Local consent is still seeded into the arm from config at startup (so the
//! gate reflects `remote_tx_enabled` even when the transport is off), matching
//! the coordinator's constructor seeding — this is idempotent.

pub mod net;

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ed25519_dalek::VerifyingKey;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use pancetta_agent::arm::{ArmEffect, ArmState, DisarmReason};
use pancetta_agent::audit::{AuditEvent, AuditKind, AuditLog};
use pancetta_agent::capability::CapabilityVerifier;
use pancetta_agent::control::{map_client_frame, ControlAction, TxKind};
use pancetta_agent::keys::AgentIdentity;
use pancetta_agent::multi_session::{MultiPeerSession, Poll};
use pancetta_agent::pairing::{IdpKey, PairedState};

use crate::message_bus::{
    ComponentId, ComponentMessage, MessageBus, MessageType, RigControlMessage,
};

/// Reconnect backoff (capped) after a transient session teardown.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_secs(2);
/// Cap on the reconnect backoff.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Bounded poll tick for `MultiPeerSession::process_next` — the demux is
/// driven in a loop rather than an unbounded blocking recv (Task 4), so a
/// `Quiet` tick is a normal, frequent event (not an error) that lets the loop
/// also service future per-tick work (Task 6's read-stream drain).
const RECV_TICK: Duration = Duration::from_millis(200);

/// Everything the dispatch loop needs to act on a decrypted control action:
/// the shared arm, the capability verifier, the client's device verifying key,
/// the station-local TX-allow-list, the session replay set, and the audit log.
///
/// Held on the blocking session task; the arm is the coordinator's shared
/// `remote_tx_arm`, so arming here is visible to the TX worker immediately.
struct ArmContext {
    arm: Arc<Mutex<ArmState>>,
    verifier: CapabilityVerifier,
    /// Verifying keys for allow-listed clients, keyed by client keyId. Shared
    /// and periodically refreshed by the Q-0043 auto-populate poll task
    /// (Task 4) in lockstep with `tx_allow_list` — a keyId appearing in the
    /// allow-list without its verifying key loaded here fails signature
    /// verification, so the two must never update independently.
    client_keys: Arc<std::sync::RwLock<std::collections::HashMap<String, VerifyingKey>>>,
    /// Station-local **connection-admission** allow-list: scope-blind, this
    /// is the general peer-admission gate `MultiPeerSession` checks before
    /// allocating ANY Noise handshake state, for every peer regardless of
    /// what scope they'll eventually use. Shared and periodically refreshed
    /// by the Q-0043 auto-populate poll task (Task 4) when cqdx integration
    /// is enabled — a fail-closed poll failure never clears this, only a
    /// successful poll replaces its contents. When cqdx integration is
    /// disabled, this is seeded once from config and never changes (today's
    /// original behavior, preserved).
    ///
    /// CRITICAL (CQD-11 / dispensa Q-0052): a client-key-id appearing here
    /// does NOT mean it may arm TX — a cross-user delegated `status`/`qsy`
    /// guest edge (CQD-4) legitimately belongs in this set (it must be able
    /// to CONNECT) but must never be able to arm TX. See `tx_arm_allow_list`
    /// below for the TX-eligible subset; `verify_and_arm` MUST gate on that
    /// set, never on this one.
    tx_allow_list: Arc<std::sync::RwLock<HashSet<String>>>,
    /// Station-local **TX-arm-eligible** subset of `tx_allow_list` (CQD-11 /
    /// dispensa Q-0052): every client keyId here is also in `tx_allow_list`,
    /// but `tx_allow_list` may contain additional keyIds (delegated
    /// `status`/`qsy` guests, CQD-4) that are NOT in this set. Populated by
    /// the same Q-0043 auto-populate poll task, filtered additionally on
    /// `delegated_by.is_none()` (an edge with no `delegatedBy` is the station
    /// owner's own client edge, not a delegated one — `role` is NOT usable
    /// for this, see `AuthorizationEdge::role`'s doc). `verify_and_arm` is the
    /// ONLY reader; this is defense-in-depth on top of the independent,
    /// IdP-signed `capabilityToken` check there (cqdx's capability-mint
    /// endpoint already hard-refuses to mint a `tx`-scoped token for a
    /// delegated edge) — never a replacement for it. When cqdx integration is
    /// disabled, this is seeded once from config identically to
    /// `tx_allow_list` (no delegation concept exists in static-config mode,
    /// so every statically-configured client stays TX-eligible, preserving
    /// today's original behavior byte-for-byte for non-cqdx-integrated
    /// stations).
    tx_arm_allow_list: Arc<std::sync::RwLock<HashSet<String>>>,
    /// Arm-time **best-effort** revocation deny-list, keyed by the
    /// capabilityToken's `jti` (frozen e2e-auth.v1 §6-revocation). **EMPTY in
    /// v1** — the station-local TX-allow-list is the authoritative revoke, and
    /// an empty deny-list is inert (never blocks). SEAM: a future cqdx-fed
    /// deny-list (fetched/pushed over the station's cqdx link) would populate
    /// this set so a leaked long-lived enabled token can be fast-revoked; it
    /// fails OPEN (stays empty) when the station is offline, per the contract's
    /// "best-effort" posture.
    revoked_jtis: HashSet<String>,
    seen_jtis: HashSet<String>,
    audit: AuditLog,
    /// Per-established-peer session context, keyed by the peer's
    /// relay-DO-authenticated, station-locally-vetted client keyId.
    ///
    /// An entry exists here ONLY for a peer `MultiPeerSession` reported as
    /// `PeerEstablished` — that variant fires only AFTER `MultiPeerSession`'s
    /// own admission check (station-local `tx_allow_list` membership + the
    /// `MAX_PEERS` capacity gate) ran BEFORE any handshake state was
    /// allocated, so a key in this map always names a vetted, allow-listed
    /// peer, never a bare relay claim. Cleared at the start of every new
    /// session (`run_one_session`) — no established peer or granted scope
    /// carries across a reconnect, so a different set of allow-listed clients
    /// can connect next time.
    peers: std::collections::HashMap<String, PeerCtx>,
    /// The single peer (by keyId) currently allowed to DRIVE the radio (arm TX,
    /// change frequency, initiate a QSO). At most one at a time (Task 5). `None`
    /// = nobody holds control yet — the first control-mutating action implicitly
    /// grabs it (rule 3). A peer that is not the controller is refused any
    /// control-mutating action (rule 4), but `Disarm`/`Heartbeat` are accepted
    /// from ANY established peer regardless (fail-safe TX-OFF, rule 5). Cleared
    /// when the controller leaves (rule 6) or on any session teardown.
    controller: Option<String>,
    /// Set by `poll_authorizations_loop` (round-1 review fix, PR #261) the
    /// instant a live authorization refresh disarms the current controller
    /// (`DisarmReason::AuthorizationRevoked`) — the poll task runs in a
    /// SEPARATE tokio task from the session loop that owns the actual
    /// websocket sends, so it cannot call `control_state_sends`/
    /// `deliver_sends` itself; it can only flag the transition and let the
    /// session loop's own tick pick it up. `run_one_session` checks (and
    /// clears) this once per tick and, if set, broadcasts the CURRENT
    /// (already-disarmed) control state to every established peer — every
    /// OTHER arm transition in this module already does this immediately as
    /// part of the transition itself; this is what makes a poll-driven
    /// revocation match that same rule instead of leaving already-connected
    /// peers holding a stale `transmitArmed: true` until their next
    /// heartbeat/TX request happens to refresh it. Purely a display-
    /// freshness signal — the TX worker enforces the real gate by reading
    /// `ArmState::tx_permitted` live, so this flag never affects whether TX
    /// is actually permitted, only how promptly connected peers are told.
    revocation_pending: Arc<std::sync::atomic::AtomicBool>,
}

/// Per-established-peer session state (Task 4: one entry per demuxed peer,
/// replacing the single-peer-era `ArmContext::hello_scopes` scalar).
struct PeerCtx {
    /// Scopes granted by THIS peer's most recent verified
    /// `hello.capabilityToken` (`None` = no scoped action served — v1
    /// back-compat default, and the fail-closed state after any
    /// invalid/mismatched token). A `hello`'s `capabilityToken.clientKeyId`
    /// MUST equal the ACTUAL sending peer (the `peer` parameter threaded from
    /// `MultiPeerSession`'s demux, never a session-global value) before its
    /// scopes are honored (Q-0019 #5) — this is what roots read/qsy
    /// authorization in the E2E-connected, allow-listed identity of the frame
    /// that carried the token, rather than relay admission alone or any OTHER
    /// peer's identity in a multi-peer session.
    hello_scopes: Option<Vec<String>>,
}

/// rig-api.v1 scope hierarchy (Q-0019 #5): `status` ⊂ `qsy` ⊂ `tx` — a token
/// scoped for a higher tier implicitly covers every lower tier. Returns
/// whether `scopes` grants at least `required`.
fn scope_at_least(scopes: &[String], required: &str) -> bool {
    let tier = |s: &str| match s {
        "status" => 1,
        "qsy" => 2,
        "tx" => 3,
        _ => 0,
    };
    let required_tier = tier(required);
    scopes
        .iter()
        .any(|s| tier(s) >= required_tier && required_tier > 0)
}

/// Apply the accumulated [`ArmEffect`]s from an `ArmState` transition: write
/// each `Audit` record to the durable log. `Disarmed` effects need no extra
/// coordinator signal here — the TX worker consults `tx_permitted()` live at
/// key-time, so a disarmed arm is enforced without an explicit stand-down msg.
fn apply_arm_effects(audit: &AuditLog, effects: &[ArmEffect]) {
    for e in effects {
        match e {
            ArmEffect::Audit(ev) => audit.append(ev),
            ArmEffect::Disarmed { reason } => {
                debug!(target: "agent.tx", reason = ?reason, "remote arm disarmed");
            }
            ArmEffect::HeartbeatRejected { reason } => {
                warn!(target: "agent.tx", reason = %reason, "remote heartbeat rejected — window NOT slid");
            }
        }
    }
}

/// Unix milliseconds now (the one clock read for arm timing; `ArmState` is pure).
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Last-resort fail-safe for the live station-agent task. The guard is bound
/// before session state is constructed and therefore disarms during unwinding
/// from any panic in the session loop. Drop must remain synchronous and
/// panic-free: it can itself run while another panic is already unwinding.
struct RemoteTxDisarmGuard {
    arm: Arc<Mutex<ArmState>>,
    audit: AuditLog,
}

impl Drop for RemoteTxDisarmGuard {
    fn drop(&mut self) {
        let effects = match self.arm.lock() {
            Ok(mut state) => state.disarm_with_reason(DisarmReason::ComponentCrash, now_ms()),
            Err(poisoned) => poisoned
                .into_inner()
                .disarm_with_reason(DisarmReason::ComponentCrash, now_ms()),
        };
        apply_arm_effects(&self.audit, &effects);
    }
}

/// The outcome of dispatching one decrypted control action — whether the caller
/// should keep the session alive (`Continue`) or tear it down and disarm
/// (`Teardown`, e.g. peer-down presence). Read-stream sends are handled by the
/// caller; this focuses on arm + rig-control side effects.
#[derive(Debug, PartialEq, Eq)]
enum Dispatch {
    /// Keep processing.
    Continue,
    /// The control channel is lost — disarm and reconnect.
    Teardown,
}

/// Who a [`PeerSend`] is addressed to. Every dispatch-emitted frame is
/// per-receiver ([`SendTarget::One`]) because `controlHeldByMe` differs per
/// peer; the read-stream fan-out does NOT go through `PeerSend` — it calls
/// [`MultiPeerSession::broadcast`] directly (see [`drain_read_stream`]).
#[derive(Debug)]
enum SendTarget {
    /// Exactly one established peer (by keyId).
    One(String),
}

/// A server→client frame the dispatch decided to emit, plus its routing. The
/// dispatch stays PURE with respect to the websocket — it only *describes* the
/// sends; [`run_one_session`] performs them (serialize + `send_to`).
#[derive(Debug)]
struct PeerSend {
    to: SendTarget,
    frame: pancetta_protocol::ServerFrame,
}

/// The full result of dispatching one control action: the arm/rig side effects
/// already applied, the flow decision (`Continue`/`Teardown`), and the set of
/// per-peer frames the caller must deliver.
#[derive(Debug)]
struct DispatchOutcome {
    flow: Dispatch,
    sends: Vec<PeerSend>,
}

impl DispatchOutcome {
    /// `Continue` with no frames to send (the common no-op path).
    fn cont() -> Self {
        DispatchOutcome {
            flow: Dispatch::Continue,
            sends: Vec::new(),
        }
    }
    /// `Continue` carrying `sends`.
    fn cont_with(sends: Vec<PeerSend>) -> Self {
        DispatchOutcome {
            flow: Dispatch::Continue,
            sends,
        }
    }
}

/// Per-receiver control/arm state frames (rig-api.v1 `controlState`). Emitted on
/// every controller transition, every successful arm, every disarm, and as a
/// greeting to each newly-established peer. `only = Some(p)` targets just peer
/// `p` (the greeting case); `None` fans out one tailored frame per established
/// peer (each with its own `controlHeldByMe`).
fn control_state_sends(ctx: &ArmContext, now: i64, only: Option<&str>) -> Vec<PeerSend> {
    let armed = ctx.arm.lock().map(|s| s.tx_permitted(now)).unwrap_or(false);
    ctx.peers
        .keys()
        .filter(|p| only.is_none_or(|o| o == p.as_str()))
        .map(|p| PeerSend {
            to: SendTarget::One(p.clone()),
            frame: pancetta_protocol::ServerFrame::Event {
                event: pancetta_protocol::ServerEvent::ControlState {
                    control_held_by_me: ctx.controller.as_deref() == Some(p.as_str()),
                    transmit_armed: armed,
                },
            },
        })
        .collect()
}

/// The outcome of the one-controller-at-a-time gate for a control-mutating
/// action (`Arm`/`Qsy`/`SetSplit`/`TxRequest`/`StopCq`).
enum ControllerGate {
    /// The peer may proceed. Carries any `controlState` frames from an implicit
    /// grab (empty when the peer was already the controller).
    Proceed(Vec<PeerSend>),
    /// The peer is NOT the controller — the action is refused; carries the
    /// `Error` frame to send back to just that peer.
    Refused(Vec<PeerSend>),
}

/// Apply rules 3 (implicit grab) + 4 (exclusivity) for a control-mutating
/// action from `peer`:
/// - no controller yet → `peer` implicitly grabs control (+ `controlState`),
///   then proceeds;
/// - `peer` already holds control → proceed with no extra frames;
/// - a DIFFERENT peer holds control → refuse (audit `TxDenied` + `Error` frame
///   to `peer`), do NOT execute and do NOT grab.
fn controller_gate(ctx: &mut ArmContext, peer: &str, now: i64) -> ControllerGate {
    match ctx.controller.as_deref() {
        None => {
            // Rule 3 — implicit grab keeps a single connected client (today's
            // only real-world case, which never sends takeControl) working with
            // zero behavior change.
            ctx.controller = Some(peer.to_string());
            debug!(target: "agent.control", peer = %peer, "implicit control grab (no controller set)");
            ControllerGate::Proceed(control_state_sends(ctx, now, None))
        }
        Some(p) if p == peer => ControllerGate::Proceed(Vec::new()),
        Some(_) => {
            // Rule 4 — exclusivity.
            warn!(target: "agent.control", peer = %peer, "control action refused — another client holds control");
            ctx.audit.append(&AuditEvent {
                ts_unix_ms: now,
                kind: AuditKind::TxDenied,
                operator_callsign: None,
                detail: "refused: not controller".to_string(),
            });
            ControllerGate::Refused(vec![PeerSend {
                to: SendTarget::One(peer.to_string()),
                frame: pancetta_protocol::ServerFrame::Event {
                    event: pancetta_protocol::ServerEvent::Error {
                        component: "stationAgent".into(),
                        message: "another client holds control — takeControl first".into(),
                    },
                },
            }])
        }
    }
}

/// Dispatch one decrypted control action against the arm + coordinator bus.
///
/// Pure with respect to timing (takes `now_ms`); side effects are the arm
/// mutation, audit writes, and best-effort bus sends. This is the security spine
/// of the component and is unit-tested directly.
///
/// - `Arm { capability_token, grant }` → verify the capabilityToken as a
///   SEPARATE input (against the pinned IdP key) then verify the client-signed
///   grant (fail-closed, including the `txEnabledUntil` clock-2 gate + the
///   best-effort deny-list) → `arm.arm()` on success, audited `TxDenied` on any
///   verification error (NEVER arms on failure). The token is NOT extracted from
///   the grant — `clientSig` signs only the grant, which references the token
///   via `capabilityJti`.
/// - `Heartbeat` → `arm.heartbeat(arm_jti, seq)`: slides the dead-man window
///   only for a heartbeat that names the current arm (`arm_jti`) with a
///   per-arm-monotonic `seq`; a replayed/wrong-arm heartbeat is rejected (audited
///   `TxDenied`) and the window is NOT slid (contract `$defs.txHeartbeat`).
/// - `Disarm { arm_jti }` → `arm.disarm()` + audit (fail-safe TX-OFF). The
///   `arm_jti` is a sanity match, NOT a security gate: disarm-any always
///   proceeds; a non-empty arm_jti that doesn't match the live arm still
///   disarms but logs a `warn!`.
/// - `Qsy` / `SetSplit` → coordinator `RigControlMessage` (NON-TX rig control).
/// - `TxRequest(_)` → audited `TxRequested` + routed into the REAL QSO engine
///   via a `remote_origin = true` `QsoMessage` (P3.4c). Creating the QSO is
///   allowed; TRANSMISSION is gated — every TransmitRequest the QSO emits is
///   `TxOrigin::Remote` and dropped by the armed-TX gate unless armed. This
///   arm never keys TX directly (no bypass).
/// - `StopCq` / `TakeControl` / `ReleaseControl` / `Unsupported` → logged no-op
///   in v1.
async fn dispatch_action(
    action: ControlAction,
    peer: &str,
    ctx: &mut ArmContext,
    bus: &MessageBus,
    session_id: &str,
    now: i64,
) -> DispatchOutcome {
    match action {
        ControlAction::Arm {
            capability_token,
            grant,
        } => {
            // Rule 3/4: arming is a control-mutating action — gate on the
            // controller first (implicit grab if unset; refuse a non-controller).
            let grab = match controller_gate(ctx, peer, now) {
                ControllerGate::Proceed(sends) => sends,
                ControllerGate::Refused(sends) => return DispatchOutcome::cont_with(sends),
            };
            match verify_and_arm(&capability_token, &grant, peer, ctx, session_id, now) {
                Ok(()) => {
                    // Rule 7: a successful arm emits controlState (transmit_armed
                    // now reflects the live arm) to every peer, appended after any
                    // implicit-grab frames.
                    let mut sends = grab;
                    sends.extend(control_state_sends(ctx, now, None));
                    DispatchOutcome::cont_with(sends)
                }
                Err(reason) => {
                    // Fail-closed: audit the denial, do NOT arm. Any implicit-grab
                    // controlState still stands (the grab itself succeeded).
                    ctx.audit.append(&AuditEvent {
                        ts_unix_ms: now,
                        kind: AuditKind::TxDenied,
                        operator_callsign: None,
                        detail: format!("arm rejected: {reason}"),
                    });
                    warn!(target: "agent.tx", reason = %reason, "remote arm grant rejected");
                    DispatchOutcome::cont_with(grab)
                }
            }
        }
        ControlAction::Heartbeat { arm_jti, seq } => {
            // Rule 5: heartbeats are accepted from ANY established peer — NO
            // controller check. `ArmState`'s arm_jti + monotonic-seq binding
            // already scopes a heartbeat to whoever holds the matching jti, and
            // cross-peer Noise isolation (Task 3) means another peer can't learn
            // that jti by eavesdropping.
            //
            // Bind the heartbeat to the armed grant it names (arm_jti) and enforce
            // per-arm `seq` monotonicity (contract $defs.txHeartbeat): a replayed
            // or wrong-arm heartbeat is rejected and does NOT slide the dead-man
            // window, so a captured heartbeat can never hold an arm open.
            let effects = match ctx.arm.lock() {
                Ok(mut st) => st.heartbeat(&arm_jti, seq, now),
                Err(_) => Vec::new(),
            };
            apply_arm_effects(&ctx.audit, &effects);
            DispatchOutcome::cont()
        }
        ControlAction::Disarm { arm_jti } => {
            // Disarm is fail-safe TX-OFF: it ALWAYS proceeds. The frozen
            // e2e-auth.v1 $defs.txDisarm.armJti is a sanity match, not a gate —
            // a non-empty arm_jti that doesn't name the live arm still disarms,
            // but we warn so a mismatch is visible in the audit trail.
            if !arm_jti.is_empty() {
                let matches_live = ctx
                    .arm
                    .lock()
                    .ok()
                    .and_then(|s| s.current_arm_jti().map(|j| j == arm_jti))
                    .unwrap_or(false);
                if !matches_live {
                    warn!(
                        target: "agent.tx",
                        arm_jti = %arm_jti,
                        "txDisarm armJti mismatch (disarming anyway — fail-safe)"
                    );
                }
            }
            // Rule 5: disarm is accepted from ANY established peer — NO
            // controller check (fail-safe TX-OFF beats exclusivity). Does not
            // change who holds control.
            let effects = match ctx.arm.lock() {
                Ok(mut st) => st.disarm(now),
                Err(_) => Vec::new(),
            };
            apply_arm_effects(&ctx.audit, &effects);
            // Rule 7: every disarm emits controlState (transmit_armed now false).
            DispatchOutcome::cont_with(control_state_sends(ctx, now, None))
        }
        ControlAction::Hello { capability_token } => {
            // Rule 8: Hello scope verification is per-peer and unchanged from
            // Task 4 — it is NOT a control-mutating action and never grabs.
            dispatch_hello(capability_token, peer, ctx, now);
            DispatchOutcome::cont()
        }
        ControlAction::Qsy { vfo, frequency_hz } => {
            let grab = match controller_gate(ctx, peer, now) {
                ControllerGate::Proceed(sends) => sends,
                ControllerGate::Refused(sends) => return DispatchOutcome::cont_with(sends),
            };
            if !ctx
                .peers
                .get(peer)
                .and_then(|p| p.hello_scopes.as_deref())
                .is_some_and(|s| scope_at_least(s, "qsy"))
            {
                warn!(target: "agent.control", peer = %peer, "setFrequency refused — no qsy-scoped hello token this session for this peer");
                return DispatchOutcome::cont_with(grab);
            }
            let msg = RigControlMessage::SetFrequency {
                vfo: vfo.clamp(0, u8::MAX as i64) as u8,
                frequency: frequency_hz.max(0.0) as u64,
            };
            send_rig(bus, msg).await;
            DispatchOutcome::cont_with(grab)
        }
        ControlAction::SetSplit {
            enabled,
            tx_frequency_hz,
        } => {
            let grab = match controller_gate(ctx, peer, now) {
                ControllerGate::Proceed(sends) => sends,
                ControllerGate::Refused(sends) => return DispatchOutcome::cont_with(sends),
            };
            if !ctx
                .peers
                .get(peer)
                .and_then(|p| p.hello_scopes.as_deref())
                .is_some_and(|s| scope_at_least(s, "qsy"))
            {
                warn!(target: "agent.control", peer = %peer, "setSplit refused — no qsy-scoped hello token this session for this peer");
                return DispatchOutcome::cont_with(grab);
            }
            let msg = RigControlMessage::SetSplit {
                enabled,
                tx_frequency: tx_frequency_hz.max(0.0) as u64,
            };
            send_rig(bus, msg).await;
            DispatchOutcome::cont_with(grab)
        }
        ControlAction::TxRequest(kind) => {
            let grab = match controller_gate(ctx, peer, now) {
                ControllerGate::Proceed(sends) => sends,
                ControllerGate::Refused(sends) => return DispatchOutcome::cont_with(sends),
            };
            // P3.4c: route the remote operator's TX-initiation through the REAL
            // QSO engine, tagged `remote_origin = true` so every TransmitRequest
            // the QSO emits is `TxOrigin::Remote` and therefore gated by the
            // armed-TX gate at pickup + key-time (P2/P3).
            //
            // SECURITY: creating the QSO is NOT the gated act — TRANSMISSION is.
            // We never key TX here. An unarmed remote operator's QSO is created
            // but every frame it emits is dropped by the TX worker's arm gate
            // (because it is `TxOrigin::Remote`) — logged (`warn!`, target
            // "agent.tx"), surfaced to the local TUI Diagnostics overlay
            // (`DiagnosticEvent`), AuditLog-appended (`AuditKind::TxDenied`,
            // matching this action's own `Arm`-rejection audit below), and
            // relayed to any connected remote client as a generic `error`
            // event (`MessageType::TxDenied` → `ServerEvent::Error`) — all
            // three closed by dispensa Q-0051. There is deliberately NO code
            // path here that keys TX outside the normal
            // QSO → MessageToSend → TransmitRequest(Remote) → arm-gate flow.
            let detail = match &kind {
                TxKind::CallStation { callsign, .. } => format!("callStation {callsign}"),
                TxKind::AnswerCaller { callsign, step, .. } => {
                    format!("answerCaller {callsign} step={step}")
                }
                TxKind::StartCq { offset_hz } => format!("startCq offset={offset_hz}"),
            };
            ctx.audit.append(&AuditEvent {
                ts_unix_ms: now,
                kind: AuditKind::TxRequested,
                operator_callsign: ctx
                    .arm
                    .lock()
                    .ok()
                    .and_then(|s| s.operator_callsign().map(str::to_string)),
                detail: detail.clone(),
            });
            let qso_msg = tx_kind_to_qso_message(kind);
            send_qso(bus, qso_msg).await;
            info!(
                target: "agent.tx",
                request = %detail,
                "routed remote TX-initiation to the QSO engine (remote_origin=true, arm-gated)"
            );
            DispatchOutcome::cont_with(grab)
        }
        ControlAction::StopCq => {
            // A control-mutating action (rules 3/4) that is otherwise a no-op in
            // v1: still gate on the controller (implicit grab / refuse) so it
            // can't be driven by a non-controller.
            match controller_gate(ctx, peer, now) {
                ControllerGate::Proceed(sends) => {
                    debug!(target: "agent.control", "stopCq accepted (not wired in v1)");
                    DispatchOutcome::cont_with(sends)
                }
                ControllerGate::Refused(sends) => DispatchOutcome::cont_with(sends),
            }
        }
        ControlAction::TakeControl => {
            // Rule 1: free grab from ANY established peer always succeeds. If it
            // displaces a DIFFERENT controller, disarm that controller's arm
            // FIRST (arms never transfer — the new controller must arm fresh).
            let displacing_other =
                ctx.controller.is_some() && ctx.controller.as_deref() != Some(peer);
            if displacing_other {
                let effects = match ctx.arm.lock() {
                    Ok(mut st) => st.disarm(now),
                    Err(_) => Vec::new(),
                };
                if !effects.is_empty() {
                    debug!(target: "agent.control", peer = %peer, "takeControl displaced a controller with a live arm — disarming first");
                }
                apply_arm_effects(&ctx.audit, &effects);
            }
            ctx.controller = Some(peer.to_string());
            info!(target: "agent.control", peer = %peer, "control taken (free grab)");
            DispatchOutcome::cont_with(control_state_sends(ctx, now, None))
        }
        ControlAction::ReleaseControl => {
            // Rule 2: from the current controller → disarm if armed + clear
            // controller. From anyone else → debug no-op, no state change.
            if ctx.controller.as_deref() == Some(peer) {
                let effects = match ctx.arm.lock() {
                    Ok(mut st) => st.disarm(now),
                    Err(_) => Vec::new(),
                };
                apply_arm_effects(&ctx.audit, &effects);
                ctx.controller = None;
                info!(target: "agent.control", peer = %peer, "control released");
                DispatchOutcome::cont_with(control_state_sends(ctx, now, None))
            } else {
                debug!(target: "agent.control", peer = %peer, "releaseControl from a non-controller — no-op");
                DispatchOutcome::cont()
            }
        }
        ControlAction::Unsupported => {
            debug!(target: "agent.control", "ignoring unsupported control frame");
            DispatchOutcome::cont()
        }
    }
}

/// Handle a `hello.capabilityToken` (Q-0019 #5): verify it against the pinned
/// IdP keys, bind it to the ACTUAL SENDING PEER's E2E-connected client
/// identity (`peer` — the value `MultiPeerSession`'s demux reported alongside
/// this specific frame, never a session-global value), and set that peer's
/// `PeerCtx::hello_scopes` accordingly. Fail-closed: any missing/invalid/
/// mismatched token clears scopes (served nothing scoped), never partially
/// trusts one.
///
/// `capability_token: None` (a legacy hello, v1 back-compat) also clears
/// scopes — the token is optional on the wire but required at runtime for any
/// scoped action.
///
/// SECURITY (multi-peer rebinding): the comparison is `cap.client_key_id ==
/// peer`, NOT against any field stored on `ctx` — `ctx` is shared across every
/// concurrently-connected peer, so a check against session-global state would
/// let peer B's hello (correctly Noise-authenticated as B by the demux) be
/// accepted under a capabilityToken that claims to be peer A, if A happened to
/// be the value some stale/shared field held. Binding to the per-frame `peer`
/// parameter is what keeps each peer's authorization strictly its own.
fn dispatch_hello(capability_token: Option<String>, peer: &str, ctx: &mut ArmContext, now: i64) {
    let Some(token) = capability_token else {
        if let Some(p) = ctx.peers.get_mut(peer) {
            p.hello_scopes = None;
        }
        return;
    };
    match ctx.verifier.verify_capability_token(&token, now) {
        Ok(cap) if cap.client_key_id == peer => {
            debug!(
                target: "agent.control",
                peer = %peer,
                scopes = ?cap.scopes,
                "hello capabilityToken verified — scopes granted for this peer"
            );
            match ctx.peers.get_mut(peer) {
                Some(p) => p.hello_scopes = Some(cap.scopes),
                None => {
                    // Defensive: cannot happen via the demux (a Plaintext poll
                    // is only ever emitted for an established peer, and every
                    // established peer gets a PeerCtx on PeerEstablished), but
                    // fail closed rather than panic if it ever did.
                    warn!(target: "agent.control", peer = %peer, "hello verified for a peer with no PeerCtx — refusing");
                }
            }
        }
        Ok(cap) => {
            warn!(
                target: "agent.control",
                token_client = %cap.client_key_id,
                peer = %peer,
                "hello capabilityToken clientKeyId does not match the sending peer's E2E-connected identity — no scope granted"
            );
            if let Some(p) = ctx.peers.get_mut(peer) {
                p.hello_scopes = None;
            }
        }
        Err(e) => {
            warn!(target: "agent.control", peer = %peer, reason = %e, "hello capabilityToken invalid — no scope granted");
            if let Some(p) = ctx.peers.get_mut(peer) {
                p.hello_scopes = None;
            }
        }
    }
}

/// Whether `client_key_id` is CURRENTLY present in `tx_arm_allow_list` — a
/// live `RwLock` read, never a cached/stale value. Factored out so
/// `verify_and_arm`'s authoritative re-check (taken while holding
/// `ctx.arm`'s lock, immediately before `st.arm(...)`) is unit-testable in
/// isolation for the "a write lands between two calls is observed by the
/// second call" property — `verify_and_arm` itself has no `.await`/yield
/// point between its top-of-function check and this re-check, so an
/// external test cannot practically win a real race against a single call
/// to it. The actual atomicity guarantee instead comes from BOTH this
/// re-check and `poll_authorizations_loop`'s `tx_arm_allow_list` publish
/// serializing through the same `ctx.arm` mutex (see both call sites' doc
/// comments for the full linearization argument). Named `is_tx_arm_eligible`
/// (not `tx_arm_eligible`) to avoid shadowing the local variable of that
/// name in this module's own tests.
fn is_tx_arm_eligible(
    tx_arm_allow_list: &Arc<std::sync::RwLock<HashSet<String>>>,
    client_key_id: &str,
) -> bool {
    tx_arm_allow_list.read().unwrap().contains(client_key_id)
}

/// Verify the SIBLING `capabilityToken` + client-signed `txArmGrant` and arm the
/// shared `ArmState`.
///
/// Fail-closed: any verification error returns `Err(reason)` and the caller
/// audits a `TxDenied` without arming. On success, `ArmState::arm` is called
/// (which itself audits `Armed`, or refuses + audits a no-tx-scope grant).
///
/// Per the frozen e2e-auth.v1 `$defs.txArm`, `capability_token` is a **separate
/// input** from `grant` — the token is NEVER read from inside the grant (the
/// grant's `clientSig` covers only the grant fields and references the token via
/// `capabilityJti`). The token is verified against the pinned IdP keys FIRST;
/// then `verify_arm_grant` enforces the `txEnabledUntil` clock-2 gate, the
/// arm-time best-effort deny-list (`ctx.revoked_jtis`, empty/inert in v1), the
/// `clientSig`, the station-local TX-allow-list, the `sessionId` bind against
/// `session_id` (dispensa Q-0022 — the SENDING PEER's own channel-binding
/// session id, so a captured grant can't be replayed into a different session
/// OR a different peer's session), and the window/heartbeat/scope bounds. The
/// grant must carry a `clientKeyId` present in the allow-list AND for which we
/// hold a device verifying key.
///
/// SECURITY (multi-peer rebinding, the cross-peer-replay blocker): the grant's
/// claimed `clientKeyId` is checked against `peer` — the ACTUAL DEMUXED SENDER
/// of this specific `txArm` frame, as reported by `MultiPeerSession` — NOT
/// against any field stored on `ctx`. `ctx` is shared across every
/// concurrently-connected peer, so a grant naming peer A's `clientKeyId` must
/// be refused unless THIS frame was demuxed as coming from A; a check against
/// session-global state instead could let peer B's frame be honored under
/// peer A's identity, defeating per-peer isolation.
fn verify_and_arm(
    capability_token: &str,
    grant: &serde_json::Value,
    peer: &str,
    ctx: &mut ArmContext,
    session_id: &str,
    now: i64,
) -> Result<(), String> {
    let obj = grant.as_object().ok_or("grant is not a JSON object")?;

    // The client keyId this grant claims — used to pick the device key AND
    // gate on the station-local allow-list before any crypto.
    let client_key_id = obj
        .get("clientKeyId")
        .and_then(|v| v.as_str())
        .ok_or("grant missing clientKeyId")?;
    // CRITICAL: the grant's claimed identity MUST equal the actual demuxed
    // sender of this frame — never trust the peer's own claim in isolation,
    // and never compare against any ctx-level/session-global value (which
    // would be shared across every peer in this multiplexed session).
    if client_key_id != peer {
        return Err(format!(
            "grant clientKeyId {client_key_id} does not match the sending peer {peer}"
        ));
    }
    // CQD-11 / dispensa Q-0052: gate on the TX-ARM-ELIGIBLE subset, not the
    // (scope-blind) connection-admission `tx_allow_list` — a delegated
    // `status`/`qsy` guest edge (CQD-4) belongs in the admission set (it must
    // be able to connect) but must NEVER reach this point successfully.
    let allow_list = ctx.tx_arm_allow_list.read().unwrap();
    if !allow_list.contains(client_key_id) {
        return Err(format!(
            "client {client_key_id} not in station-local TX-arm-allow-list"
        ));
    }
    let client_keys = ctx.client_keys.read().unwrap();
    let client_vk = *client_keys
        .get(client_key_id)
        .ok_or_else(|| format!("no device key for client {client_key_id}"))?;

    // Verify the capabilityToken as a SEPARATE input (frozen e2e-auth.v1
    // $defs.txArm: token + grant are siblings; the token is NOT trusted from
    // inside the grant). This also runs the short-TTL / enablement backstops.
    let cap = ctx
        .verifier
        .verify_capability_token(capability_token, now)
        .map_err(|e| format!("capability: {e}"))?;

    let verified = ctx
        .verifier
        .verify_arm_grant(
            grant,
            &cap,
            &client_vk,
            &allow_list,
            &ctx.revoked_jtis,
            session_id,
            now,
            &mut ctx.seen_jtis,
        )
        .map_err(|e| format!("arm grant: {e}"))?;

    // CRITICAL (deadlock avoidance): drop the top-of-function
    // `tx_arm_allow_list` read guard NOW, before acquiring `ctx.arm`'s lock
    // below. NLL lets the compiler treat `allow_list` as "no longer
    // borrowed" after its last use above, but that is a BORROW-CHECK
    // relaxation only — it does NOT change when the guard's `Drop` actually
    // runs, which is still tied to lexical scope (function end) unless
    // dropped explicitly. Left implicit, this guard would stay held while
    // the re-check below calls `is_tx_arm_eligible`, which tries to acquire
    // a SECOND (recursive) read lock on the SAME `RwLock` on this same
    // thread. `std::sync::RwLock` is not guaranteed reentrant, and —
    // critically — is writer-preferring enough on common platforms
    // (Darwin's pthread_rwlock among them) that once a writer
    // (`poll_authorizations_loop`'s publish) is queued waiting for this
    // thread's first read to release, a SECOND read from the SAME thread
    // blocks behind that queued writer too, which itself is waiting on the
    // first read to release — a genuine self-deadlock.
    drop(allow_list);

    // Arm the shared state (audits Armed, or refuses a no-scope grant).
    let effects = match ctx.arm.lock() {
        Ok(mut st) => {
            // CQD-11 / dispensa Q-0052 follow-up (atomicity fix): the
            // top-of-function `tx_arm_allow_list` read above (and the copy
            // `verify_arm_grant` just checked internally) can be STALE by
            // the time we get here — token/grant verification takes no
            // lock, so a concurrent `poll_authorizations_loop` cycle could
            // revoke this exact client's eligibility in the meantime.
            // `poll_authorizations_loop` publishes its refreshed
            // `tx_arm_allow_list` WHILE STILL HOLDING `ctx.arm`'s lock (see
            // there), so re-checking membership here — under the SAME lock
            // we just acquired — is what makes "top check, then arm" atomic
            // with "disarm-check, then publish": whichever side acquires
            // `arm` second always observes the other's fully-published
            // result, never a stale in-between state.
            if !is_tx_arm_eligible(&ctx.tx_arm_allow_list, client_key_id) {
                return Err(format!(
                    "client {client_key_id} not in station-local TX-arm-allow-list \
                     (revoked by a concurrent authorization refresh)"
                ));
            }
            st.arm(verified, now)
        }
        Err(_) => return Err("arm mutex poisoned".to_string()),
    };
    apply_arm_effects(&ctx.audit, &effects);
    Ok(())
}

/// Best-effort forward of a rig-control message onto the coordinator bus.
async fn send_rig(bus: &MessageBus, msg: RigControlMessage) {
    let m = ComponentMessage::new(
        ComponentId::StationAgent,
        ComponentId::Hamlib,
        MessageType::RigControl(msg),
        std::time::Instant::now(),
    );
    if let Err(e) = bus.send_message(m).await {
        debug!(target: "agent.control", "rig-control forward failed: {e}");
    }
}

/// Forward a QSO-control message to the QSO component. Used to route a remote
/// operator's TX-initiation into the real QSO engine. The messages carry
/// `remote_origin = true`, so the QSO's TransmitRequests are `TxOrigin::Remote`
/// and armed-TX gated downstream (this call itself keys nothing).
async fn send_qso(bus: &MessageBus, msg: crate::message_bus::QsoMessage) {
    let m = ComponentMessage::new(
        ComponentId::StationAgent,
        ComponentId::Qso,
        MessageType::QsoMessage(msg),
        std::time::Instant::now(),
    );
    if let Err(e) = bus.send_message(m).await {
        debug!(target: "agent.control", "qso-control forward failed: {e}");
    }
}

/// Parse the rig-api.v1 `dxParity` wire string (`"even"` / `"odd"`) into a
/// [`SlotParity`]. Any other value (or `None`) → `None` (the QSO scheduler
/// falls back to its self-parity default), which is the safe, non-colliding
/// choice when the client did not supply a parity.
fn parse_dx_parity(s: Option<&str>) -> Option<pancetta_core::slot::SlotParity> {
    match s {
        Some("even") => Some(pancetta_core::slot::SlotParity::Even),
        Some("odd") => Some(pancetta_core::slot::SlotParity::Odd),
        _ => None,
    }
}

/// Parse the rig-api.v1 `step` wire string into a [`ResponseStep`]. Unknown
/// values default to [`ResponseStep::Grid`] (the historical opening reply),
/// matching the engine's own default.
fn parse_response_step(s: &str) -> pancetta_core::ResponseStep {
    use pancetta_core::ResponseStep;
    match s {
        "report" => ResponseStep::Report,
        "reportAck" => ResponseStep::ReportAck,
        "rr73" => ResponseStep::Rr73,
        "seventyThree" => ResponseStep::SeventyThree,
        _ => ResponseStep::Grid,
    }
}

/// Map a decrypted remote [`TxKind`] to the [`QsoMessage`](crate::message_bus::QsoMessage)
/// that opens the corresponding QSO with `remote_origin = true`. This is the
/// single point that stamps the remote origin, so the resulting QSO's TX is
/// `TxOrigin::Remote` end to end.
fn tx_kind_to_qso_message(kind: TxKind) -> crate::message_bus::QsoMessage {
    use crate::message_bus::QsoMessage;
    match kind {
        TxKind::CallStation {
            callsign,
            frequency_hz,
            dx_parity,
        } => QsoMessage::StartQso {
            callsign,
            frequency: frequency_hz.max(0.0) as u64,
            dx_parity: parse_dx_parity(dx_parity.as_deref()),
            remote_origin: true,
        },
        TxKind::AnswerCaller {
            callsign,
            frequency_hz,
            step,
            dx_parity,
            snr,
        } => QsoMessage::RespondToCaller {
            callsign,
            frequency: frequency_hz.max(0.0) as u64,
            dx_parity: parse_dx_parity(dx_parity.as_deref()),
            step: parse_response_step(&step),
            snr: snr.map(|v| v as f32),
            remote_origin: true,
        },
        TxKind::StartCq { offset_hz } => QsoMessage::StartCq {
            frequency: offset_hz.max(0.0) as u64,
            tx_parity: None,
            remote_origin: true,
        },
    }
}

/// Whether the station agent is configured to attempt a relay connection at
/// all: enabled, with both `relay_url`/`pairing_api_url` present and
/// non-empty. A config-only, filesystem-free check — it does NOT prove the
/// station is actually *paired* (that needs `PairedState::load`, a
/// filesystem read `start_station_agent_component` performs later) or that
/// identity keys load successfully. Factored out so
/// [`station_agent_active`] (which adds the allow-list check) shares this
/// exact condition with `start_station_agent_component`'s own gating,
/// instead of two copies of the same boolean drifting apart.
fn has_relay_config(cfg: &pancetta_config::network::StationAgentConfig) -> bool {
    cfg.enabled
        && cfg.relay_url.as_deref().is_some_and(|s| !s.is_empty())
        && cfg
            .pairing_api_url
            .as_deref()
            .is_some_and(|s| !s.is_empty())
}

/// Whether the station agent WILL (config permitting) want the shared
/// [`display feed`](super::remote_gateway::DisplayFeed): [`has_relay_config`]
/// AND (a non-empty station-local TX-allow-list OR cqdx auto-populate
/// enabled with a token, dispensa Q-0043) — the same gates
/// `start_station_agent_component` itself checks before ever attempting a
/// relay connection. Deliberately does NOT check pairing state (filesystem)
/// — an over-approximation (starting the feed for a config that turns out
/// unpaired at runtime) is harmless; the failure mode this predicate exists
/// to prevent is the opposite one, a station agent that needs the feed
/// finding it was never started. `start_display_feed` calls this SAME
/// function for its own gating so the two can never drift apart.
///
/// The cqdx branch matters for Q-0043's steady state: with auto-populate
/// enabled, the operator deliberately leaves `tx_allow_list` empty in
/// config (cqdx is the source of truth) — without this branch, this
/// predicate would (wrongly) say "inactive" even though the poll task will
/// populate the allow-list moments after the component starts, and the
/// display feed would never start for the whole process lifetime.
pub(crate) fn station_agent_active(
    cfg: &pancetta_config::network::StationAgentConfig,
    cqdx_cfg: &pancetta_config::network::CqdxConfig,
) -> bool {
    let cqdx_will_populate =
        cqdx_cfg.enabled && cqdx_cfg.token.as_ref().is_some_and(|t| !t.is_empty());
    has_relay_config(cfg) && (!cfg.tx_allow_list.is_empty() || cqdx_will_populate)
}

/// The initial seed for `tx_arm_allow_list` — FAIL-CLOSED, not fail-open
/// (CQD-11 / dispensa Q-0052 follow-up: PR #259 shipped the split with a
/// fail-open seed; this closes that gap).
///
/// When `cqdx_will_poll` (cqdx auto-populate enabled with a token — the same
/// condition that gates spawning `poll_authorizations_loop`), TX-arm
/// eligibility starts EMPTY: nobody can arm TX until a successful poll
/// establishes the non-delegated subset. A client statically listed in
/// `tx_allow_list` must NOT be able to arm before the first poll completes,
/// nor indefinitely if cqdx stays unreachable — that would let a client whose
/// edge has since become delegated reuse a still-valid enabled token after a
/// restart, defeating the whole connection-admission/TX-arm split.
///
/// When cqdx auto-populate is NOT going to run, there is no delegation
/// concept at all in the pure-config path, so `tx_arm_allow_list` clones
/// `static_tx_allow_list` and (absent a poll task to ever diverge it) stays
/// permanently equal to it — this is the pre-existing, intended behavior for
/// non-cqdx-integrated stations, unchanged.
fn seed_tx_arm_allow_list(
    static_tx_allow_list: &HashSet<String>,
    cqdx_will_poll: bool,
) -> HashSet<String> {
    if cqdx_will_poll {
        HashSet::new()
    } else {
        static_tx_allow_list.clone()
    }
}

impl super::ApplicationCoordinator {
    /// Definitively close the shared remote-TX arm after StationAgent loss.
    ///
    /// Poison is cleared only after the session has been removed, preserving
    /// the fail-closed mutex behavior throughout recovery.
    pub(crate) async fn fail_safe_disarm_remote_tx(&self, why: &str) {
        let effects = match self.remote_tx_arm.lock() {
            Ok(mut state) => state.disarm_with_reason(DisarmReason::ComponentCrash, now_ms()),
            Err(poisoned) => poisoned
                .into_inner()
                .disarm_with_reason(DisarmReason::ComponentCrash, now_ms()),
        };
        apply_arm_effects(&self.audit_log(), &effects);
        drop(effects);
        self.remote_tx_arm.clear_poison();

        crate::coordinator::tx::emit_diagnostic(
            &self.message_bus,
            "agent.tx",
            pancetta_core::DiagnosticLevel::Error,
            format!("Remote TX disarmed: {why}"),
            None,
        )
        .await;
    }

    /// Start the station-agent component (default-OFF, inert unless enabled +
    /// paired). Mirrors [`start_remote_gateway_component`](super::ApplicationCoordinator::start_remote_gateway_component):
    /// disabled/unpaired → drain-only; enabled + paired → connect + serve.
    ///
    /// `--replay` ([`super::ApplicationCoordinator::replay_mode`]) forces the
    /// same drain-only path a disabled component takes, before any key
    /// material is loaded and before the relay connection is ever attempted.
    /// This closes BOTH directions at once, which is why it is an early return
    /// rather than merely passing `events: None` into `run_session_loop`:
    ///
    /// - OUTBOUND: `drain_read_stream` serializes every display-feed
    ///   `ServerEvent` and `MultiPeerSession::broadcast`s it to every
    ///   established peer over the relay socket — real off-box network traffic
    ///   carrying replayed decodes, spectrum, QSO state and dial frequency
    ///   fabricated as live.
    /// - INBOUND: an established peer's control frames (`dispatch_action` →
    ///   `send_rig`/`send_qso`) would otherwise drive QSY/QSO actions against a
    ///   demo process. Replay already skips Hamlib, so there is no PTT
    ///   capability, but a remote control surface has no business existing
    ///   during a demo at all.
    ///
    /// Local consent seeding (above) still runs: it only mirrors config into
    /// the in-process arm, touches nothing off-box, and keeps the arm's value
    /// identical to a live run's — with no session to admit a controller, it
    /// can never lead to TX.
    pub(crate) async fn start_station_agent_component(&mut self) -> Result<()> {
        let config = self.config.read().await;
        let cfg = config.network.station_agent.clone();
        let cqdx_cfg = config.network.cqdx.clone();
        // WSJT-X-UDP design spec Option A: the shared remote-TX arm is seeded by
        // EITHER channel's operator consent. `set_local_consent` OVERWRITES, so
        // this later seed must carry the combined value — otherwise it would
        // clobber a wsjtx-contributed arm back to `remote_tx_enabled` alone.
        //
        // The wsjtx contribution is gated on `enabled` too (mirrors the
        // coordinator-constructor seeding): with the component disabled there
        // is no Reply-handling path, so an armed-but-unreachable seed from
        // `allow_tx_initiation` alone is pure risk with no benefit.
        let wsjtx_allow_tx_initiation =
            config.network.wsjtx_udp.enabled && config.network.wsjtx_udp.allow_tx_initiation;
        drop(config);

        // Seed local consent from config regardless of enabled/paired, so the
        // arm reflects the combined `remote_tx_enabled || allow_tx_initiation`
        // consent even when the station-agent transport is off. This mirrors
        // (idempotently) the coordinator-constructor seeding.
        {
            let now = now_ms();
            let consent = crate::coordinator::wsjtx_udp::remote_tx_arm_consent(
                cfg.remote_tx_enabled,
                wsjtx_allow_tx_initiation,
            );
            if let Ok(mut st) = self.remote_tx_arm.lock() {
                let _ = st.set_local_consent(consent, now);
            }
        }

        // --- Inert path: --replay (see this function's doc) -------------------
        // Checked before key loading / pairing state so a demo run never even
        // touches the agent key dir, let alone opens the relay socket.
        if self.replay_mode() {
            info!(
                "Replay mode: station_agent not started -- replayed decodes are not live traffic, \
                 and no remote peer may drive a demo process"
            );
            return self.spawn_station_agent_drain().await;
        }

        // --- Inert paths: disabled or missing required config ---------------
        let (relay_url, pairing_api_url) = match (&cfg.relay_url, &cfg.pairing_api_url) {
            (Some(r), Some(p)) if has_relay_config(&cfg) => (r.clone(), p.clone()),
            _ => {
                if cfg.enabled {
                    info!("station_agent enabled but relay_url/pairing_api_url missing — inert");
                } else {
                    info!("station_agent disabled in configuration");
                }
                return self.spawn_station_agent_drain().await;
            }
        };
        let _ = pairing_api_url; // pairing is an operator CLI action (not auto-run here).

        // --- Load identity + paired state -----------------------------------
        let key_dir = cfg
            .key_dir
            .clone()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(default_key_dir);

        let identity = match AgentIdentity::load_or_generate(&key_dir) {
            Ok(id) => id,
            Err(e) => {
                warn!(target: "agent", "failed to load/generate agent identity: {e}; inert");
                return self.spawn_station_agent_drain().await;
            }
        };

        let paired = match PairedState::load(&key_dir) {
            Ok(p) => p,
            Err(_) => {
                warn!(
                    target: "agent",
                    "station agent enabled but not paired — run pairing (operator action); staying idle, relay connection never attempted"
                );
                return self.spawn_station_agent_drain().await;
            }
        };

        // Build the capability verifier from the pinned IdP keys.
        let verifier = CapabilityVerifier {
            agent_key_id: paired.agent_key_id.clone(),
            pinned_idp_keys: paired.idp_keys.clone(),
        };

        // The station-local TX-allow-list + the (client keyId → device key) map.
        // v1 has no device-key registry beyond the allow-list of keyIds; the
        // client's device verifying key is not known until pairing extends it.
        // For P3.4b the client device keys are supplied via the allow-list AND
        // an (optional) sidecar — absent that, the grant's clientSig cannot be
        // checked, so an un-registered client fails closed at verify time.
        let tx_allow_list: HashSet<String> = cfg.tx_allow_list.iter().cloned().collect();
        let client_keys = load_client_device_keys(&key_dir, &tx_allow_list);
        // CQD-11 / dispensa Q-0052 follow-up: the TX-arm-eligible seed must
        // be FAIL-CLOSED, not fail-open. This condition mirrors the poll-task
        // spawn gate below EXACTLY (`cqdx_cfg.enabled` + a non-empty token) —
        // when cqdx auto-populate WILL run, `tx_arm_allow_list` starts EMPTY:
        // nobody can arm TX until a successful poll proves who's
        // non-delegated. Cloning the static list here (the original PR #259
        // approach) lets every statically-listed client arm before the first
        // poll succeeds — and indefinitely if cqdx stays unreachable,
        // including reusing a still-valid enabled token after a restart for
        // a client whose edge has since become delegated. `tx_allow_list`
        // (connection admission) is UNCHANGED — a delegated guest can still
        // connect from the first second, just never arm. Only the
        // cqdx-DISABLED path (no delegation concept at all) clones the
        // static list, matching pre-existing behavior for non-cqdx-
        // integrated stations, permanently.
        let cqdx_will_poll =
            cqdx_cfg.enabled && cqdx_cfg.token.as_ref().is_some_and(|t| !t.is_empty());
        let tx_arm_allow_list = Arc::new(std::sync::RwLock::new(seed_tx_arm_allow_list(
            &tx_allow_list,
            cqdx_will_poll,
        )));
        let tx_allow_list = Arc::new(std::sync::RwLock::new(tx_allow_list));
        let client_keys = Arc::new(std::sync::RwLock::new(client_keys));

        if !station_agent_active(&cfg, &cqdx_cfg) {
            warn!(
                target: "agent",
                "station agent paired but tx_allow_list is empty and cqdx auto-populate is \
                 disabled — no client to admit; idle, relay connection never attempted"
            );
            return self.spawn_station_agent_drain().await;
        }

        // Shared with the TX worker (dispensa Q-0051 Phase B) — single source
        // of truth, constructed once at coordinator startup from the same
        // `cfg.audit_log_path` resolution this used to do locally.
        let audit = self.audit_log();

        // Task 4: `run_session_loop`/`run_one_session` now drives a
        // `MultiPeerSession`, which can demux up to `multi_session::MAX_PEERS`
        // (8) concurrently-connected allow-listed clients over the one relay
        // socket, each with its own independent Noise transport and its own
        // `PeerCtx` (granted scopes). `MultiPeerSession`'s own admission check
        // — station-local `tx_allow_list` membership + the capacity gate, run
        // BEFORE any handshake state is allocated — remains the authoritative
        // gate (mirroring `verify_and_arm`'s TX-time check); relay admission
        // alone never grants anything. N=1 parity: v1 still admits only one
        // ARMED controller at a time (Task 5 adds that arbitration) — but
        // several allow-listed peers may now hold independent read/qsy scope
        // concurrently. An empty allow-list still means no client can ever be
        // admitted.
        let bus = self.message_bus.clone();
        let shutdown = self.shutdown_signal.clone();
        let arm = self.remote_tx_arm.clone();
        // Round-1 review fix (PR #261): shared with `poll_authorizations_loop`
        // so a poll-driven disarm can flag the session loop to broadcast the
        // updated control state (see `ArmContext::revocation_pending`).
        let revocation_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Task 6: subscribe to the shared display feed (if the localhost
        // gateway or this component's own gating started it — see
        // `start_display_feed`/`station_agent_active`) so the read stream has
        // something to drain. `None` when the feed never started (inert —
        // `drain_read_stream` is a no-op on `None`).
        let events = self.display_feed.as_ref().map(|f| f.evt_tx.subscribe());

        // Drain channel so additive bus sends addressed to StationAgent never
        // flood (parity with the gateway).
        let (_sa_tx, _sa_rx) = self
            .message_bus
            .get_or_create_channel(ComponentId::StationAgent)
            .await?;

        // Same condition as `cqdx_will_poll` above (kept as one boolean, not
        // recomputed, so the seed-fail-closed decision and the spawn
        // decision can never drift apart).
        if cqdx_will_poll {
            let poll_tx_allow_list = tx_allow_list.clone();
            let poll_tx_arm_allow_list = tx_arm_allow_list.clone();
            let poll_client_keys = client_keys.clone();
            let poll_key_dir = key_dir.clone();
            let poll_agent_key_id = paired.agent_key_id.clone();
            // Disarm-on-revocation: the shared arm + audit log so the poll
            // task can disarm immediately when a refresh revokes the live
            // controller's TX-arm eligibility (see `poll_authorizations_loop`
            // doc).
            let poll_arm = arm.clone();
            let poll_audit = audit.clone();
            let poll_revocation_pending = revocation_pending.clone();
            let poll_shutdown = self.shutdown_signal.clone();
            let poll_interval = Duration::from_secs(cqdx_cfg.authorizations_poll_interval_secs);
            let poll_handle = tokio::spawn(async move {
                poll_authorizations_loop(
                    cqdx_cfg,
                    poll_agent_key_id,
                    poll_key_dir,
                    poll_tx_allow_list,
                    poll_tx_arm_allow_list,
                    poll_client_keys,
                    poll_arm,
                    poll_audit,
                    poll_revocation_pending,
                    poll_shutdown,
                    poll_interval,
                )
                .await;
            });
            self.station_agent_poll = Some(poll_handle.abort_handle());
        }

        let handle = tokio::spawn(async move {
            run_session_loop(RunConfig {
                relay_url,
                identity,
                verifier,
                client_keys,
                tx_allow_list,
                tx_arm_allow_list,
                audit,
                bus,
                events,
                arm,
                revocation_pending,
                shutdown,
            })
            .await;
            Ok::<(), anyhow::Error>(())
        });
        self.named_task_handles
            .push((ComponentId::StationAgent, handle));
        info!("station_agent component started (paired; connecting to relay)");
        Ok(())
    }

    /// Spawn the no-op drain task for the inert (off/unpaired) path.
    async fn spawn_station_agent_drain(&mut self) -> Result<()> {
        let (_drain_tx, drain_rx) = self
            .message_bus
            .get_or_create_channel(ComponentId::StationAgent)
            .await?;
        let shutdown = self.shutdown_signal.clone();
        let handle = tokio::spawn(async move {
            while !shutdown.load(Ordering::Acquire) {
                loop {
                    match drain_rx.try_recv() {
                        Ok(_) => {}
                        Err(crossbeam_channel::TryRecvError::Empty) => break,
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            return Ok::<(), anyhow::Error>(());
                        }
                    }
                }
                sleep(Duration::from_millis(100)).await;
            }
            Ok(())
        });
        self.named_task_handles
            .push((ComponentId::StationAgent, handle));
        Ok(())
    }
}

/// The default agent key directory: `~/.pancetta/agent`. Also used by the
/// `pancetta pair` CLI subcommand so both paths resolve the same directory.
pub fn default_key_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".pancetta")
        .join("agent")
}

/// Load client device verifying keys from `key_dir/clients/<keyId>.pub` (raw
/// 32-byte Ed25519), for each allow-listed keyId. Missing/invalid files are
/// skipped (that client then fails closed at verify time). Populated by the
/// pairing CLI (P3.4c); absent it, the map is empty and no client can arm.
fn load_client_device_keys(
    key_dir: &std::path::Path,
    allow: &HashSet<String>,
) -> std::collections::HashMap<String, VerifyingKey> {
    let mut out = std::collections::HashMap::new();
    let dir = key_dir.join("clients");
    for kid in allow {
        // keyIds are base64url (may contain '/','+','=' in padded form, but the
        // agent keyId form is unpadded base64url — no '/'); guard the filename.
        let safe: String = kid
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let path = dir.join(format!("{safe}.pub"));
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
                if let Ok(vk) = VerifyingKey::from_bytes(&arr) {
                    out.insert(kid.clone(), vk);
                }
            }
        }
    }
    out
}

/// Periodically refresh the shared `tx_allow_list`/`tx_arm_allow_list`/
/// `client_keys` from cqdx's live authorization data (dispensa Q-0043
/// auto-populate; the `tx_arm_allow_list` split is CQD-11 / dispensa Q-0052).
/// Runs for the lifetime of the station-agent component; only spawned when
/// cqdx integration is enabled with a token configured (see
/// `start_station_agent_component`).
///
/// Fail-safe: any poll failure (network error, non-2xx status including a
/// 404, malformed JSON) is logged at WARN and the shared state is left
/// UNTOUCHED — a transient cqdx outage (relevant right now: cqdx.io's
/// production deploy is currently down) must never spuriously revoke an
/// already-admitted, connected client. Only a successful poll (a 200 with a
/// parseable body, even if `authorizations` is genuinely empty) replaces
/// the shared contents.
///
/// Disarm-on-revocation + atomic publish (CQD-11 / dispensa Q-0052
/// follow-up): `verify_and_arm`'s `tx_arm_allow_list` gate only runs at ARM
/// time — it does nothing for an arm that is already live when a LATER poll
/// revokes its controller's eligibility (edge dropped entirely, or newly
/// delegated). The frozen e2e-auth.v1 §6-revocation invariant is "the cloud
/// may only ever reduce station-local TX authority", so a live arm whose
/// controller falls out of the freshly-computed `tx_arm_allow_list` is
/// disarmed immediately here, rather than left to run out its grant TTL (up
/// to 60 minutes). The disarm-check AND the `tx_arm_allow_list` publish
/// happen under ONE `arm.lock()` critical section — matching
/// `verify_and_arm`'s own re-check under that SAME lock (see there) — so a
/// `verify_and_arm` call that read the OLD list at its top can never race to
/// acquire `arm.lock()` in a window between this section's disarm-check and
/// the publish and arm a just-revoked client with nothing left to revisit it
/// until the NEXT poll cycle.
///
/// Round-1 review fix (PR #261): this task runs in a SEPARATE tokio task
/// from the session loop that owns the actual websocket sends, so it cannot
/// call `control_state_sends`/`deliver_sends` itself when it disarms. It
/// instead flags `revocation_pending` (mirrors `ArmContext::
/// revocation_pending` — the SAME `Arc`), which `run_one_session` checks and
/// clears once per tick, broadcasting the updated control state to every
/// established peer then — matching this module's existing rule that every
/// arm transition fans `control_state_sends` out immediately.
#[allow(clippy::too_many_arguments)]
async fn poll_authorizations_loop(
    cqdx_cfg: pancetta_config::network::CqdxConfig,
    agent_key_id: String,
    key_dir: std::path::PathBuf,
    tx_allow_list: Arc<std::sync::RwLock<HashSet<String>>>,
    tx_arm_allow_list: Arc<std::sync::RwLock<HashSet<String>>>,
    client_keys: Arc<std::sync::RwLock<std::collections::HashMap<String, VerifyingKey>>>,
    arm: Arc<Mutex<ArmState>>,
    audit: AuditLog,
    revocation_pending: Arc<std::sync::atomic::AtomicBool>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    interval: Duration,
) {
    let Some(token) = cqdx_cfg.token.clone() else {
        return; // Defensive — caller already checked this before spawning.
    };
    let client = match pancetta_cqdx::CqdxClient::new(cqdx_cfg.base_url.clone(), token) {
        Ok(c) => c,
        Err(e) => {
            warn!(target: "agent", "authorizations poll: failed to construct cqdx client: {e}; poll task exiting");
            return;
        }
    };

    while !shutdown.load(Ordering::Acquire) {
        match client.fetch_authorizations().await {
            Ok(edges) => {
                // Connection-admission set: scope-blind, `agentKeyId` only —
                // a delegated status/qsy guest edge (CQD-4) legitimately
                // belongs here (it must be able to connect).
                let new_allow: HashSet<String> = edges
                    .iter()
                    .filter(|e| e.agent_key_id == agent_key_id)
                    .map(|e| e.client_key_id.clone())
                    .collect();
                // TX-arm-eligible subset (CQD-11 / dispensa Q-0052): the SAME
                // `agentKeyId` filter, PLUS `delegated_by.is_none()` — an edge
                // with no `delegatedBy` is the station owner's own client
                // edge, never a cross-user-delegated one. `role` is
                // deliberately NOT used here (see `AuthorizationEdge::role`'s
                // doc — it reads "owner" on every row this poll's own
                // `agentKeyId` filter keeps, so it distinguishes nothing).
                let new_tx_arm_allow: HashSet<String> = edges
                    .iter()
                    .filter(|e| e.agent_key_id == agent_key_id && e.delegated_by.is_none())
                    .map(|e| e.client_key_id.clone())
                    .collect();
                let new_keys = load_client_device_keys(&key_dir, &new_allow);
                let allow_len = new_allow.len();
                let tx_arm_len = new_tx_arm_allow.len();

                // Disarm-on-revocation + atomic publish: see the fn-level
                // doc above. Poisoned-lock handling mirrors
                // `RemoteTxDisarmGuard::drop` — a poisoned arm mutex must
                // never block publishing the refreshed TX-arm-eligible set.
                {
                    let mut st = match arm.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    let should_disarm = st
                        .armed_client_key_id()
                        .is_some_and(|cur| !new_tx_arm_allow.contains(cur));
                    if should_disarm {
                        let effects =
                            st.disarm_with_reason(DisarmReason::AuthorizationRevoked, now_ms());
                        apply_arm_effects(&audit, &effects);
                        // Flag the session loop (a SEPARATE tokio task, so it
                        // cannot be signaled by directly calling
                        // control_state_sends/deliver_sends here) to
                        // broadcast the updated control state on its next
                        // tick — see the fn-level doc above.
                        revocation_pending.store(true, Ordering::Release);
                        warn!(
                            target: "agent.tx",
                            "remote TX disarmed: an authorization refresh revoked the \
                             current controller's TX-arm eligibility"
                        );
                    }
                    // Published WHILE STILL HOLDING `arm`'s lock — this is
                    // the atomicity guarantee `verify_and_arm`'s re-check
                    // relies on.
                    *tx_arm_allow_list.write().unwrap() = new_tx_arm_allow;
                }

                *tx_allow_list.write().unwrap() = new_allow;
                *client_keys.write().unwrap() = new_keys;
                debug!(
                    target: "agent",
                    "authorizations poll: refreshed tx_allow_list ({} client(s) admitted for this agent, {} TX-arm-eligible)",
                    allow_len, tx_arm_len
                );
            }
            Err(e) => {
                warn!(
                    target: "agent",
                    "authorizations poll failed: {e} — keeping last-known-good tx_allow_list/tx_arm_allow_list, retrying in {:?}",
                    interval
                );
            }
        }
        tokio::time::sleep(interval).await;
    }
}

/// Everything the session loop owns.
struct RunConfig {
    relay_url: String,
    identity: AgentIdentity,
    verifier: CapabilityVerifier,
    client_keys: Arc<std::sync::RwLock<std::collections::HashMap<String, VerifyingKey>>>,
    tx_allow_list: Arc<std::sync::RwLock<HashSet<String>>>,
    tx_arm_allow_list: Arc<std::sync::RwLock<HashSet<String>>>,
    audit: AuditLog,
    bus: MessageBus,
    /// The shared display-feed subscription (Task 6), if the feed was
    /// started. `None` when neither the localhost gateway nor this
    /// component's own gating (`station_agent_active`) needed it — in which
    /// case the read stream is permanently inert for this run
    /// (`drain_read_stream` no-ops on `None`).
    events: Option<tokio::sync::broadcast::Receiver<pancetta_protocol::ServerEvent>>,
    arm: Arc<Mutex<ArmState>>,
    /// Threaded into `ArmContext::revocation_pending` — see there.
    revocation_pending: Arc<std::sync::atomic::AtomicBool>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

/// The connect → authenticate → process → (disarm on loss) → reconnect loop.
///
/// On any session teardown the arm is disarmed (fail TX-off on control-channel
/// loss, Part-97), then the loop reconnects with capped backoff until shutdown.
async fn run_session_loop(mut cfg: RunConfig) {
    let _disarm_guard = RemoteTxDisarmGuard {
        arm: cfg.arm.clone(),
        audit: cfg.audit.clone(),
    };
    let mut backoff = RECONNECT_BACKOFF_MIN;
    let mut ctx = ArmContext {
        arm: cfg.arm.clone(),
        verifier: cfg.verifier,
        client_keys: cfg.client_keys,
        tx_allow_list: cfg.tx_allow_list,
        tx_arm_allow_list: cfg.tx_arm_allow_list,
        // v1: no cqdx-fed deny-list yet (empty ⇒ inert; the station-local
        // TX-allow-list is the authoritative revoke). Future seam: populate
        // this from a cqdx revocation feed on (re)connect.
        revoked_jtis: HashSet::new(),
        seen_jtis: HashSet::new(),
        audit: cfg.audit,
        // Not pinned to any fixed peer — established peers are learned and
        // vetted fresh each session by `MultiPeerSession` itself (see
        // `run_one_session`), so a different set of allow-listed clients can
        // connect on the next reconnect.
        peers: std::collections::HashMap::new(),
        // No controller until a peer grabs it (explicitly or implicitly).
        controller: None,
        revocation_pending: cfg.revocation_pending,
    };

    while !cfg.shutdown.load(Ordering::Acquire) {
        match net::RealWsConn::connect(&cfg.relay_url).await {
            Ok(ws) => {
                backoff = RECONNECT_BACKOFF_MIN;
                run_one_session(ws, &cfg.identity, &mut ctx, &cfg.bus, &mut cfg.events).await;
                // Session ended (teardown / drained): fail TX-off.
                disarm_on_loss(&mut ctx);
            }
            Err(e) => {
                debug!(target: "agent", "relay connect failed: {e}");
            }
        }
        // Backoff before reconnect (respect shutdown).
        if cfg.shutdown.load(Ordering::Acquire) {
            break;
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
    // Final safety: disarm on component shutdown.
    disarm_on_loss(&mut ctx);
}

/// Disarm the shared arm on any control-channel loss and audit it. Also clears
/// the controller — a fresh session (or the next connecting peer) must grab
/// control again; control never carries across a session teardown.
fn disarm_on_loss(ctx: &mut ArmContext) {
    let effects = match ctx.arm.lock() {
        Ok(mut st) => st.disarm(now_ms()),
        Err(_) => Vec::new(),
    };
    if !effects.is_empty() {
        debug!(target: "agent.tx", "control channel lost — disarming remote TX");
    }
    apply_arm_effects(&ctx.audit, &effects);
    ctx.controller = None;
}

/// Serialize and deliver each [`PeerSend`]. Pure-side-effect helper the session
/// loop calls after every dispatch (and on peer-establish/-down). A failed
/// serialize or send is `debug!`-logged and skipped (best-effort — a control
/// frame that can't reach one peer never tears down the session).
fn deliver_sends<W: pancetta_agent::relay::WsConn>(
    sess: &mut MultiPeerSession<'_, W>,
    sends: &[PeerSend],
) {
    for send in sends {
        let bytes = match serde_json::to_vec(&send.frame) {
            Ok(b) => b,
            Err(e) => {
                debug!(target: "agent.control", "controlState serialize failed: {e}");
                continue;
            }
        };
        let SendTarget::One(peer) = &send.to;
        if let Err(e) = sess.send_to(peer, &bytes) {
            debug!(target: "agent.control", peer = %peer, "peer send failed: {e}");
        }
    }
}

/// Fan pending display events out to every established peer. Lossy by design:
/// Lagged means the bounded broadcast ring dropped old events — log and continue.
fn drain_read_stream<W: pancetta_agent::relay::WsConn>(
    events: &mut Option<tokio::sync::broadcast::Receiver<pancetta_protocol::ServerEvent>>,
    sess: &mut MultiPeerSession<'_, W>,
) {
    let Some(rx) = events.as_mut() else { return };
    loop {
        match rx.try_recv() {
            Ok(ev) => {
                let frame = pancetta_protocol::ServerFrame::Event { event: ev };
                match serde_json::to_vec(&frame) {
                    Ok(bytes) => {
                        let _ = sess.broadcast(&bytes);
                    }
                    Err(e) => debug!(target: "agent", "read-stream serialize failed: {e}"),
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                debug!(target: "agent", skipped = n, "read stream lagged — events dropped");
            }
            Err(_) => break, // Empty or Closed
        }
    }
}

/// Drive one session to completion: auth → demux → dispatch control frames.
/// Returns when the session tears down (drain, teardown action, or error).
///
/// This runs on a blocking-capable task because the [`pancetta_agent::relay::WsConn`]
/// seam is synchronous (`RealWsConn` bridges via `block_on`).
///
/// Task 4: drives a [`MultiPeerSession`] rather than the single-peer
/// `AgentSession` — up to `multi_session::MAX_PEERS` allow-listed clients may
/// be concurrently established over the one relay socket, each with its own
/// independent Noise transport and its own [`PeerCtx`]. `MultiPeerSession`'s
/// own admission check (station-local `tx_allow_list` membership + capacity,
/// run BEFORE any handshake state is allocated) is what makes a `PeerEstablished`
/// peer already vetted — the station-local list, not relay admission, remains
/// the authoritative gate (mirroring `verify_and_arm`'s TX-time check). A
/// refused peer (`PeerRefused`) is isolated to itself: it never tears down the
/// whole session, so other already-established peers are unaffected. `ctx.peers`
/// is cleared at the start of every new session — no established peer or
/// granted scope carries across a reconnect.
async fn run_one_session<W: pancetta_agent::relay::WsConn>(
    ws: W,
    identity: &AgentIdentity,
    ctx: &mut ArmContext,
    bus: &MessageBus,
    events: &mut Option<tokio::sync::broadcast::Receiver<pancetta_protocol::ServerEvent>>,
) {
    ctx.peers.clear();
    let allow_snapshot = ctx.tx_allow_list.read().unwrap().clone();
    let mut sess = MultiPeerSession::new(ws, identity, allow_snapshot);
    if let Err(e) = sess.authenticate() {
        debug!(target: "agent", "relay authenticate failed: {e}");
        return;
    }
    loop {
        match sess.process_next(RECV_TICK) {
            Ok(Poll::Plaintext { peer, plaintext }) => {
                let action = match map_client_frame(&plaintext) {
                    Ok(a) => a,
                    Err(e) => {
                        debug!(target: "agent.control", "malformed control frame: {e}");
                        continue;
                    }
                };
                // `sid` is THIS SPECIFIC PEER's own channel-binding session id
                // (never a session-global value) — the exact input
                // `verify_and_arm`/`verify_arm_grant`'s sessionId bind needs to
                // block a grant captured on one peer's session from being
                // replayed into another peer's.
                let sid = sess.session_id(&peer).unwrap_or_default().to_string();
                let outcome = dispatch_action(action, &peer, ctx, bus, &sid, now_ms()).await;
                deliver_sends(&mut sess, &outcome.sends);
                // Task 6: drain the read stream after every successful dispatch
                // too, not just on Quiet/Idle — a chatty control session
                // (frequent Arm/Heartbeat/Qsy traffic) must never starve the
                // display feed of a chance to fan out.
                drain_read_stream(events, &mut sess);
                if outcome.flow == Dispatch::Teardown {
                    return;
                }
            }
            Ok(Poll::PeerEstablished { peer, .. }) => {
                ctx.peers
                    .insert(peer.clone(), PeerCtx { hello_scopes: None });
                // Rule 7: greet the newly-established peer with its current
                // control/arm state (targeted at just that peer).
                let sends = control_state_sends(ctx, now_ms(), Some(&peer));
                deliver_sends(&mut sess, &sends);
            }
            Ok(Poll::PeerRefused { peer }) => {
                warn!(target: "agent", peer = %peer,
                    "relay-admitted peer refused (not allow-listed or at capacity)");
            }
            Ok(Poll::PeerDown { peer }) => {
                // Rule 6: controller loss ⇒ disarm. Remove the peer first, then —
                // if it WAS the controller — disarm, clear the controller, and
                // broadcast the updated controlState to the remaining peers. A
                // non-controller leaving disarms nothing.
                let was_controller = ctx.controller.as_deref() == Some(peer.as_str());
                ctx.peers.remove(&peer);
                if was_controller {
                    let effects = match ctx.arm.lock() {
                        Ok(mut st) => st.disarm(now_ms()),
                        Err(_) => Vec::new(),
                    };
                    if !effects.is_empty() {
                        debug!(target: "agent.tx", peer = %peer, "controller left with a live arm — disarming");
                    }
                    apply_arm_effects(&ctx.audit, &effects);
                    ctx.controller = None;
                    let sends = control_state_sends(ctx, now_ms(), None);
                    deliver_sends(&mut sess, &sends);
                }
            }
            Ok(Poll::Idle) | Ok(Poll::Quiet) => {
                // Task 6: the common case — no control traffic this tick, so
                // this is where the read stream gets most of its chances to
                // fan out pending display events to established peers.
                drain_read_stream(events, &mut sess);
            }
            Ok(Poll::Closed) => return,
            Err(e) => {
                debug!(target: "agent", "session error: {e}");
                return;
            }
        }

        // Round-1 review fix (PR #261): a concurrent `poll_authorizations_loop`
        // cycle may have just disarmed the live controller
        // (`DisarmReason::AuthorizationRevoked`) — it flags this here via
        // `ctx.revocation_pending` because it runs in a separate tokio task
        // and cannot call `deliver_sends` itself. Check (and clear) the flag
        // once per tick, same granularity as every other per-tick concern in
        // this loop, and broadcast the CURRENT (already-disarmed) control
        // state to every established peer — matching this module's existing
        // rule that every arm transition fans `control_state_sends` out
        // immediately, so an already-connected peer's `transmitArmed` never
        // goes stale until its next heartbeat/TX request happens to refresh
        // it.
        if ctx.revocation_pending.swap(false, Ordering::AcqRel) {
            let sends = control_state_sends(ctx, now_ms(), None);
            deliver_sends(&mut sess, &sends);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};
    use pancetta_agent::arm::HEARTBEAT_TIMEOUT_MS;
    // Only the full end-to-end proof below still drives the single-peer
    // `AgentSession` directly (to exercise the real Noise/relay wiring at the
    // crypto layer); production code (`run_one_session`) now drives
    // `MultiPeerSession` instead (see the module-level import).
    use pancetta_agent::session::AgentSession;
    use serde_json::{json, Value};
    use std::collections::BTreeMap;

    const AGENT_KEY_ID: &str = "agentKeyId000000";
    const CLIENT_KEY_ID: &str = "clientKeyId00000";
    const IDP_KID: &str = "idp-kid-1";
    const OPERATOR: &str = "K5ARH";
    const NOW: i64 = 1_700_000_000_000;
    /// Matches `signed_grant`'s baked-in `sessionId` — the "live session"
    /// `dispatch_action` calls in this module check the grant against.
    const SESSION_ID: &str = "sess-1";

    fn b64url(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }
    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }
    fn idp_key() -> SigningKey {
        key(0x11)
    }
    fn client_key() -> SigningKey {
        key(0x22)
    }

    fn mint_jws(header: &Value, payload: &Value, k: &SigningKey) -> String {
        let h = b64url(&serde_json::to_vec(header).unwrap());
        let p = b64url(&serde_json::to_vec(payload).unwrap());
        let signing_input = format!("{h}.{p}");
        let sig = k.sign(signing_input.as_bytes());
        format!("{h}.{p}.{}", b64url(&sig.to_bytes()))
    }

    fn valid_token() -> String {
        // TX-enabled: txEnabledUntil present and == exp (frozen e2e-auth.v1), so
        // the arm-time require_tx_enabled gate passes.
        let header = json!({ "alg": "EdDSA", "kid": IDP_KID, "typ": "JWT" });
        let payload = json!({
            "iss": "cqdx", "sub": "acct-1", "operatorCallsign": OPERATOR,
            "aud": AGENT_KEY_ID, "clientKeyId": CLIENT_KEY_ID,
            "scopes": ["status", "qsy", "tx"],
            "iat": NOW / 1000 - 10, "exp": NOW / 1000 + 600,
            "txEnabledUntil": NOW / 1000 + 600, "jti": "cap-jti-1"
        });
        mint_jws(&header, &payload, &idp_key())
    }

    /// A capabilityToken with NO `txEnabledUntil` — verifies for status/qsy but
    /// must NEVER arm (require_tx_enabled → NotTxEnabled). Kept inside the 900s
    /// short-TTL cap so token verification itself succeeds.
    fn non_enabled_token() -> String {
        let header = json!({ "alg": "EdDSA", "kid": IDP_KID, "typ": "JWT" });
        let payload = json!({
            "iss": "cqdx", "sub": "acct-1", "operatorCallsign": OPERATOR,
            "aud": AGENT_KEY_ID, "clientKeyId": CLIENT_KEY_ID,
            "scopes": ["status", "qsy", "tx"],
            "iat": NOW / 1000 - 10, "exp": NOW / 1000 + 600, "jti": "cap-jti-1"
        });
        mint_jws(&header, &payload, &idp_key())
    }

    fn canonical_bytes(grant: &serde_json::Map<String, Value>) -> Vec<u8> {
        let sorted: BTreeMap<String, Value> = grant
            .iter()
            .filter(|(k, _)| k.as_str() != "clientSig")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        serde_json::to_vec(&sorted).unwrap()
    }

    /// Domain-separation tag for txArmGrant.clientSig (dispensa Q-0019 #6,
    /// 2026-07-02). Mirrors `pancetta_agent::capability::TX_ARM_GRANT_DOMAIN_TAG`.
    const TX_ARM_GRANT_DOMAIN_TAG: &str = "cqdx-tx-arm-grant-v1";

    fn domain_separated(canon: &[u8]) -> Vec<u8> {
        let mut out = TX_ARM_GRANT_DOMAIN_TAG.as_bytes().to_vec();
        out.push(0x00);
        out.extend_from_slice(canon);
        out
    }

    /// Build a valid, client-signed `txArmGrant` (frozen e2e-auth.v1: the token
    /// is a SIBLING carried separately, NOT inside the grant), with a unique jti
    /// so replay tests can vary it.
    fn signed_grant(jti: &str) -> Value {
        let mut grant = json!({
            "aud": AGENT_KEY_ID,
            "clientKeyId": CLIENT_KEY_ID,
            "sessionId": "sess-1",
            "capabilityJti": "cap-jti-1",
            "operatorCallsign": OPERATOR,
            "armedAt": NOW,
            "armedUntil": NOW + 300_000,
            "heartbeatIntervalSec": 10,
            "jti": jti
        })
        .as_object()
        .unwrap()
        .clone();
        let canon = canonical_bytes(&grant);
        let sig = client_key().sign(&domain_separated(&canon));
        grant.insert("clientSig".to_string(), json!(b64url(&sig.to_bytes())));
        Value::Object(grant)
    }

    /// A convenience: the standard `ControlAction::Arm` (token sibling + grant)
    /// used by most dispatch tests.
    fn arm_action(jti: &str) -> ControlAction {
        ControlAction::Arm {
            capability_token: valid_token(),
            grant: signed_grant(jti),
        }
    }

    /// A `peers` map with a single established entry for `peer`, carrying
    /// `hello_scopes` — the Task-4 replacement for the old single-scalar
    /// `expected_client_key_id`/`hello_scopes` test setup.
    fn peers_with(
        peer: &str,
        hello_scopes: Option<Vec<String>>,
    ) -> std::collections::HashMap<String, PeerCtx> {
        let mut m = std::collections::HashMap::new();
        m.insert(peer.to_string(), PeerCtx { hello_scopes });
        m
    }

    fn ctx_with(allow_client: bool, have_device_key: bool) -> ArmContext {
        let mut allow = HashSet::new();
        if allow_client {
            allow.insert(CLIENT_KEY_ID.to_string());
        }
        let mut client_keys = std::collections::HashMap::new();
        if have_device_key {
            client_keys.insert(CLIENT_KEY_ID.to_string(), client_key().verifying_key());
        }
        // `allow_client` here always means an OWNER edge (not a delegated
        // one) — every existing caller of `ctx_with` predates delegation and
        // expects the client to be both connection-admitted AND TX-arm
        // eligible, so both sets get the same contents.
        let tx_arm_allow = allow.clone();
        ArmContext {
            arm: Arc::new(Mutex::new(ArmState::new())),
            verifier: CapabilityVerifier {
                agent_key_id: AGENT_KEY_ID.to_string(),
                pinned_idp_keys: vec![IdpKey {
                    kid: IDP_KID.to_string(),
                    public_key: idp_key().verifying_key().to_bytes(),
                }],
            },
            client_keys: Arc::new(std::sync::RwLock::new(client_keys)),
            tx_allow_list: Arc::new(std::sync::RwLock::new(allow)),
            tx_arm_allow_list: Arc::new(std::sync::RwLock::new(tx_arm_allow)),
            revoked_jtis: HashSet::new(),
            seen_jtis: HashSet::new(),
            audit: AuditLog::new(audit_tmp()),
            // The peer this session is Noise-connected to (mirrors the old
            // default `expected_client_key_id: CLIENT_KEY_ID`) — an
            // already-established peer with no granted scope yet.
            peers: peers_with(CLIENT_KEY_ID, None),
            controller: None,
            revocation_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn audit_tmp() -> std::path::PathBuf {
        use std::sync::atomic::AtomicU64;
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("pancetta-sa-test-{}-{n}.log", std::process::id()))
    }

    fn with_consent(ctx: &ArmContext, on: bool) {
        ctx.arm.lock().unwrap().set_local_consent(on, NOW);
    }

    // ── Case 2: a validly-signed Arm from an allow-listed client permits TX ──
    #[tokio::test]
    async fn arm_from_allowlisted_client_permits_tx() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        let d = dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            ctx.arm.lock().unwrap().tx_permitted(NOW),
            "a valid arm + consent must permit remote TX"
        );
    }

    // ── CQD-11 / dispensa Q-0052: connection-admission vs. TX-arm-eligible ──

    /// (a) An owner's own edge (no delegation involved) ends up in BOTH the
    /// connection-admission set AND the TX-arm-eligible set — `ctx_with`
    /// seeds both identically for an allow-listed client, and this proves it
    /// end-to-end through the real dispatch/arm path, not just by inspecting
    /// the sets directly.
    #[tokio::test]
    async fn owner_edge_is_both_admitted_and_tx_arm_eligible() {
        let ctx = ctx_with(true, true);
        assert!(
            ctx.tx_allow_list.read().unwrap().contains(CLIENT_KEY_ID),
            "an owner's client keyId must be in the connection-admission set"
        );
        assert!(
            ctx.tx_arm_allow_list
                .read()
                .unwrap()
                .contains(CLIENT_KEY_ID),
            "an owner's client keyId must ALSO be in the TX-arm-eligible set"
        );
        drop(ctx);

        // And the arm path itself succeeds end-to-end (mirrors
        // `arm_from_allowlisted_client_permits_tx` — kept as an explicit,
        // separately-named CQD-11 regression pin).
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        let d = dispatch_action(
            arm_action("arm-jti-owner"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            ctx.arm.lock().unwrap().tx_permitted(NOW),
            "an owner edge present in both sets must be able to arm TX"
        );
    }

    /// (b) A delegated `status`/`qsy` edge (CQD-4) — present in the
    /// connection-admission set (it must be able to CONNECT) but absent from
    /// the TX-arm-eligible set — must be refused at `verify_and_arm` even
    /// with a perfectly valid token/grant/signature/session and consent ON.
    /// This is the exact gap CQD-11 closes: before this change, `ArmContext`
    /// had only one set and this same setup would have armed successfully.
    #[tokio::test]
    async fn delegated_edge_is_admitted_but_not_tx_arm_eligible() {
        let mut ctx = ctx_with(true, true);
        // Simulate the poll having classified CLIENT_KEY_ID as a delegated
        // edge: still connection-admitted (and its device key still loaded —
        // admission doesn't change), but removed from the TX-arm-eligible set.
        ctx.tx_arm_allow_list.write().unwrap().clear();
        assert!(
            ctx.tx_allow_list.read().unwrap().contains(CLIENT_KEY_ID),
            "a delegated guest must remain connection-admitted"
        );
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        let d = dispatch_action(
            arm_action("arm-jti-delegate"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            !ctx.arm.lock().unwrap().tx_permitted(NOW),
            "a delegated edge must never be able to arm TX, even with an \
             otherwise perfectly valid token/grant/signature/session"
        );

        let log = std::fs::read_to_string(ctx.audit.path()).unwrap_or_default();
        let denied = log
            .lines()
            .filter_map(|l| serde_json::from_str::<AuditEvent>(l).ok())
            .any(|ev| ev.kind == AuditKind::TxDenied && ev.detail.contains("TX-arm-allow-list"));
        assert!(
            denied,
            "a delegated edge's rejected arm attempt must record a TxDenied \
             audit event naming the TX-arm-allow-list gate specifically"
        );
    }

    /// Dispensa Q-0022: an otherwise-perfectly-valid, correctly-signed Arm
    /// grant whose `sessionId` doesn't match the LIVE session (e.g. a grant
    /// captured on a prior connection, replayed into a new one) must never
    /// arm — proven through the real coordinator dispatch path, not just
    /// `pancetta-agent`'s unit-level `verify_arm_grant` coverage.
    #[tokio::test]
    async fn arm_with_mismatched_session_id_never_arms() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        // `arm_action`'s grant carries `sessionId: "sess-1"` (== SESSION_ID);
        // dispatch it against a DIFFERENT live session id.
        let d = dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            "a-completely-different-session",
            NOW,
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            !ctx.arm.lock().unwrap().tx_permitted(NOW),
            "a grant signed for a different session must never arm this one"
        );
    }

    // ── Q-0019 #5: hello.capabilityToken roots read/qsy authorization ───────

    #[tokio::test]
    async fn qsy_refused_without_prior_hello_token() {
        // No hello has been dispatched this session — ctx.hello_scopes is None
        // by construction (ctx_with's default) — so setFrequency must be a no-op.
        let mut ctx = ctx_with(true, true);
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let d = dispatch_action(
            ControlAction::Qsy {
                vfo: 0,
                frequency_hz: 14_074_000.0,
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            hamlib_rx.try_recv().is_err(),
            "setFrequency must be refused with no verified hello token this session"
        );
    }

    #[tokio::test]
    async fn qsy_permitted_after_valid_hello_token_from_expected_client() {
        let mut ctx = ctx_with(true, true);
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        // valid_token() carries scopes=["status","qsy","tx"] for CLIENT_KEY_ID,
        // which matches ctx_with's expected_client_key_id (CLIENT_KEY_ID).
        let d1 = dispatch_action(
            ControlAction::Hello {
                capability_token: Some(valid_token()),
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(d1.flow, Dispatch::Continue);
        let d2 = dispatch_action(
            ControlAction::Qsy {
                vfo: 0,
                frequency_hz: 14_074_000.0,
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(d2.flow, Dispatch::Continue);
        let msg = hamlib_rx
            .try_recv()
            .expect("setFrequency must be forwarded after a valid qsy-scoped hello");
        match msg.message_type {
            MessageType::RigControl(RigControlMessage::SetFrequency { frequency, .. }) => {
                assert_eq!(frequency, 14_074_000);
            }
            other => panic!("expected SetFrequency, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn qsy_never_arms_tx_even_with_a_tx_scoped_hello() {
        // Read/qsy authorization (hello.capabilityToken) and TX authorization
        // (txArm) are independent gates — a qsy-scoped session must never
        // touch the ArmState.
        let mut ctx = ctx_with(true, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            ControlAction::Hello {
                capability_token: Some(valid_token()),
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        dispatch_action(
            ControlAction::Qsy {
                vfo: 0,
                frequency_hz: 7_074_000.0,
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "a qsy-scoped hello must never arm TX — arm requires a separate txArm"
        );
    }

    #[tokio::test]
    async fn qsy_refused_when_hello_token_client_key_id_mismatches_session() {
        // A token that verifies (valid IdP signature, unexpired) but whose
        // clientKeyId does NOT match the ACTUAL SENDING PEER's E2E-connected
        // identity must grant NOTHING — this is the exact "relay-admission-
        // rooted" gap Q-0019 #5 closes: a mismatched token must not silently
        // authorize. `ANOTHER_PEER` models the physically-connected/demuxed
        // sender of this frame; `valid_token()`'s clientKeyId (CLIENT_KEY_ID)
        // does NOT equal it — the rebound check (`cap.client_key_id == peer`)
        // must refuse this exactly as the old `== ctx.expected_client_key_id`
        // check did in the single-peer world.
        const ANOTHER_PEER: &str = "someOtherClientKeyId0";
        let mut ctx = ctx_with(true, true);
        ctx.peers
            .insert(ANOTHER_PEER.to_string(), PeerCtx { hello_scopes: None });
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        dispatch_action(
            ControlAction::Hello {
                capability_token: Some(valid_token()), // clientKeyId == CLIENT_KEY_ID
            },
            ANOTHER_PEER,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        let d = dispatch_action(
            ControlAction::Qsy {
                vfo: 0,
                frequency_hz: 14_074_000.0,
            },
            ANOTHER_PEER,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            hamlib_rx.try_recv().is_err(),
            "a hello token whose clientKeyId != the E2E-connected identity must grant no scope"
        );
    }

    #[tokio::test]
    async fn qsy_permitted_by_a_non_tx_enabled_token_carrying_qsy_scope() {
        // require_tx_enabled (the clock-2 TX gate) is orthogonal to read/qsy
        // scope: a token with no txEnabledUntil still verifies and still
        // grants qsy, because qsy never touches ArmState/require_tx_enabled.
        let mut ctx = ctx_with(true, true);
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        dispatch_action(
            ControlAction::Hello {
                capability_token: Some(non_enabled_token()),
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        let d = dispatch_action(
            ControlAction::Qsy {
                vfo: 0,
                frequency_hz: 21_074_000.0,
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            hamlib_rx.try_recv().is_ok(),
            "a non-tx-enabled token with qsy scope must still permit setFrequency"
        );
    }

    #[tokio::test]
    async fn new_session_clears_stale_hello_scope() {
        // The per-session reset in run_one_session ("ctx.peers.clear()", with a
        // fresh `PeerCtx { hello_scopes: None }` inserted on the next
        // `PeerEstablished`) is exercised indirectly here: simulate a stale
        // prior grant by setting the peer's hello_scopes directly, then confirm
        // the field is what gates dispatch (i.e. a reconnect-boundary reset to
        // a fresh `PeerCtx` really does deny qsy, proving the gate reads
        // `PeerCtx::hello_scopes` and nothing else).
        let mut ctx = ctx_with(true, true);
        ctx.peers.insert(
            CLIENT_KEY_ID.to_string(),
            PeerCtx {
                hello_scopes: Some(vec!["qsy".to_string()]),
            },
        );
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        // Simulate the reconnect-boundary reset a fresh `run_one_session`
        // performs: `ctx.peers.clear()` then a brand-new `PeerCtx` on the next
        // `PeerEstablished` — same peer keyId, but no scope carried over.
        ctx.peers
            .insert(CLIENT_KEY_ID.to_string(), PeerCtx { hello_scopes: None });
        let d = dispatch_action(
            ControlAction::Qsy {
                vfo: 0,
                frequency_hz: 14_074_000.0,
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            hamlib_rx.try_recv().is_err(),
            "a reset session must not inherit a prior session's granted scope"
        );
    }

    // ── Case 4: heartbeat loss auto-disarms (dead-man) ──────────────────────
    #[tokio::test]
    async fn heartbeat_loss_disarms_after_timeout() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(ctx.arm.lock().unwrap().tx_permitted(NOW));
        // No further heartbeats: at the dead-man deadline, tx_permitted is false.
        let dead = NOW + HEARTBEAT_TIMEOUT_MS;
        assert!(
            !ctx.arm.lock().unwrap().tx_permitted(dead),
            "no heartbeat within the window must auto-deny (dead-man)"
        );
        // A heartbeat *before* the deadline slides the window.
        let mut ctx2 = ctx_with(true, true);
        with_consent(&ctx2, true);
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx2,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        dispatch_action(
            ControlAction::Heartbeat {
                arm_jti: "arm-jti-1".into(),
                seq: 1,
            },
            CLIENT_KEY_ID,
            &mut ctx2,
            &bus,
            SESSION_ID,
            NOW + 20_000,
        )
        .await;
        assert!(
            ctx2.arm
                .lock()
                .unwrap()
                .tx_permitted(NOW + 20_000 + HEARTBEAT_TIMEOUT_MS - 1),
            "a heartbeat must slide the dead-man window"
        );
    }

    // ── A stale-seq Heartbeat does NOT extend the arm (replay can't hold open) ─
    #[tokio::test]
    async fn stale_seq_heartbeat_does_not_extend_the_arm() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        // Arm at NOW; the grant's jti is "arm-jti-1" (signed_grant uses it).
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(ctx.arm.lock().unwrap().tx_permitted(NOW));

        // Accept seq 5 at NOW+5000 (slides the window to there).
        let accepted_at = NOW + 5_000;
        dispatch_action(
            ControlAction::Heartbeat {
                arm_jti: "arm-jti-1".into(),
                seq: 5,
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            accepted_at,
        )
        .await;
        let deadline = accepted_at + HEARTBEAT_TIMEOUT_MS;

        // Replay seq 5 (and a lower seq) LATER — must NOT slide the window.
        dispatch_action(
            ControlAction::Heartbeat {
                arm_jti: "arm-jti-1".into(),
                seq: 5,
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            deadline - 1,
        )
        .await;
        dispatch_action(
            ControlAction::Heartbeat {
                arm_jti: "arm-jti-1".into(),
                seq: 3,
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            deadline - 1,
        )
        .await;

        // Dead-man expires on schedule despite the later replays.
        assert!(ctx.arm.lock().unwrap().tx_permitted(deadline - 1));
        assert!(
            !ctx.arm.lock().unwrap().tx_permitted(deadline),
            "a replayed-seq heartbeat must NOT hold the arm open past the dead-man deadline"
        );
    }

    // ── A wrong-arm_jti Heartbeat is rejected and never extends the arm ──────
    #[tokio::test]
    async fn wrong_arm_jti_heartbeat_is_rejected() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        // A heartbeat naming a different arm must not slide the window.
        dispatch_action(
            ControlAction::Heartbeat {
                arm_jti: "some-other-arm".into(),
                seq: 1,
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW + 10_000,
        )
        .await;
        // Original dead-man deadline (from the arm at NOW) still governs.
        assert!(ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(NOW + HEARTBEAT_TIMEOUT_MS - 1));
        assert!(!ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(NOW + HEARTBEAT_TIMEOUT_MS));
    }

    // ── Case 5: consent OFF → even a valid Arm never permits TX ─────────────
    #[tokio::test]
    async fn consent_off_never_permits_even_with_valid_arm() {
        let mut ctx = ctx_with(true, true);
        // remote_tx_enabled is OFF (default): do NOT set consent on.
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(
            !ctx.arm.lock().unwrap().tx_permitted(NOW),
            "consent OFF must deny TX even after a valid arm"
        );
    }

    // ── Case 6: an Arm from a client NOT in the allow-list is rejected ──────
    #[tokio::test]
    async fn arm_from_unallowlisted_client_is_rejected() {
        // Client key present as a device key, but NOT in the allow-list.
        let mut ctx = ctx_with(false, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "a grant from a non-allow-listed client must NOT arm"
        );
        assert!(!ctx.arm.lock().unwrap().tx_permitted(NOW));
    }

    // ── Explicit Disarm clears a live arm ───────────────────────────────────
    #[tokio::test]
    async fn explicit_disarm_clears_arm() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(ctx.arm.lock().unwrap().tx_permitted(NOW));
        dispatch_action(
            ControlAction::Disarm {
                arm_jti: "arm-jti-1".into(),
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "explicit disarm must clear the arm"
        );
    }

    // ── Replayed jti is rejected (single-use) ───────────────────────────────
    #[tokio::test]
    async fn replayed_grant_jti_is_rejected() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        let grant = signed_grant("arm-jti-1");
        dispatch_action(
            ControlAction::Arm {
                capability_token: valid_token(),
                grant: grant.clone(),
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        // Disarm, then replay the SAME grant jti — must not re-arm.
        dispatch_action(
            ControlAction::Disarm {
                arm_jti: String::new(),
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        dispatch_action(
            ControlAction::Arm {
                capability_token: valid_token(),
                grant,
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "a replayed grant jti must be rejected (single-use)"
        );
    }

    // ── A token WITHOUT txEnabledUntil does NOT arm (clock-2 gate) ───────────
    #[tokio::test]
    async fn arm_with_non_enabled_token_never_arms() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        // A well-signed, allow-listed, correctly-bound grant — but the sibling
        // capabilityToken carries NO txEnabledUntil, so require_tx_enabled fails.
        dispatch_action(
            ControlAction::Arm {
                capability_token: non_enabled_token(),
                grant: signed_grant("arm-jti-1"),
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "a token without txEnabledUntil must NEVER arm (clock-2 gate)"
        );
        assert!(!ctx.arm.lock().unwrap().tx_permitted(NOW));
    }

    // ── txDisarm{armJti} disarms a live arm (armJti is a sanity match) ──────
    #[tokio::test]
    async fn tx_disarm_with_arm_jti_disarms() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(ctx.arm.lock().unwrap().tx_permitted(NOW));
        // A txDisarm naming the live arm disarms it.
        dispatch_action(
            ControlAction::Disarm {
                arm_jti: "arm-jti-1".into(),
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "txDisarm{{armJti}} must disarm the live arm"
        );
    }

    // ── txDisarm with a MISMATCHED armJti still disarms (fail-safe) ──────────
    #[tokio::test]
    async fn tx_disarm_mismatched_arm_jti_still_disarms() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(ctx.arm.lock().unwrap().tx_permitted(NOW));
        // A txDisarm naming a DIFFERENT arm still disarms (armJti is a sanity
        // match, not a gate — disarm-any is fail-safe TX-OFF).
        dispatch_action(
            ControlAction::Disarm {
                arm_jti: "some-other-arm".into(),
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "a mismatched txDisarm armJti must STILL disarm (fail-safe)"
        );
    }

    // ── TX-initiation routes to the QSO engine but NEVER arms (no bypass) ────
    //
    // P3.4c: a remote TxRequest is routed into the real QSO engine tagged
    // `remote_origin = true`, but the dispatch NEVER arms/keys TX directly.
    // TRANSMISSION is the gated act (the QSO's TransmitRequests are
    // `TxOrigin::Remote` and dropped by the TX worker's arm gate unless armed).
    #[tokio::test]
    async fn tx_request_routes_to_qso_engine_but_never_arms() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        // Subscribe to the QSO component's inbox so we can assert the routed msg.
        let (_qso_tx, qso_rx) = bus.create_channel(ComponentId::Qso).await.unwrap();
        let d = dispatch_action(
            ControlAction::TxRequest(TxKind::CallStation {
                callsign: "W1XYZ".into(),
                frequency_hz: 1500.0,
                dx_parity: Some("odd".into()),
            }),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        // The arm is UNTOUCHED — routing a TxRequest is never an arm/bypass.
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "routing a TX-initiation must not arm or key TX"
        );
        // A StartQso with remote_origin=true was forwarded to the QSO engine.
        let msg = qso_rx.try_recv().expect("StartQso must be routed to Qso");
        match msg.message_type {
            MessageType::QsoMessage(crate::message_bus::QsoMessage::StartQso {
                callsign,
                remote_origin,
                dx_parity,
                ..
            }) => {
                assert_eq!(callsign, "W1XYZ");
                assert!(
                    remote_origin,
                    "remote TxRequest MUST set remote_origin=true"
                );
                assert_eq!(dx_parity, Some(pancetta_core::slot::SlotParity::Odd));
            }
            other => panic!("expected StartQso, got {other:?}"),
        }
    }

    // ── Each TxKind maps to the right remote-origin QsoMessage ──────────────
    #[test]
    fn tx_kind_answer_caller_maps_to_remote_respond() {
        let msg = tx_kind_to_qso_message(TxKind::AnswerCaller {
            callsign: "K2DEF".into(),
            frequency_hz: 1200.0,
            step: "reportAck".into(),
            dx_parity: Some("even".into()),
            snr: Some(-9.0),
        });
        match msg {
            crate::message_bus::QsoMessage::RespondToCaller {
                callsign,
                step,
                remote_origin,
                dx_parity,
                ..
            } => {
                assert_eq!(callsign, "K2DEF");
                assert_eq!(step, pancetta_core::ResponseStep::ReportAck);
                assert_eq!(dx_parity, Some(pancetta_core::slot::SlotParity::Even));
                assert!(remote_origin, "answerCaller MUST be remote_origin=true");
            }
            other => panic!("expected RespondToCaller, got {other:?}"),
        }
    }

    #[test]
    fn tx_kind_start_cq_maps_to_remote_cq() {
        let msg = tx_kind_to_qso_message(TxKind::StartCq { offset_hz: 800.0 });
        match msg {
            crate::message_bus::QsoMessage::StartCq {
                frequency,
                remote_origin,
                ..
            } => {
                assert_eq!(frequency, 800);
                assert!(remote_origin, "startCq MUST be remote_origin=true");
            }
            other => panic!("expected StartCq, got {other:?}"),
        }
    }

    // ========================================================================
    // Task 5: one-controller-at-a-time (free grab, exclusivity, controlState).
    // ========================================================================

    /// A second established peer keyId used by the controller tests (distinct
    /// from `CLIENT_KEY_ID`).
    const PEER_B: &str = "peerBkeyId000000";

    /// Rule 3: a single connected client that NEVER sends `takeControl` arms
    /// exactly as before — the first control-mutating action (here `Arm`)
    /// implicitly grabs control. This is the zero-behavior-change back-compat
    /// path for today's only real-world case.
    #[tokio::test]
    async fn implicit_grab_on_first_arm_single_client_compat() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        let out = dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(out.flow, Dispatch::Continue);
        assert!(
            ctx.arm.lock().unwrap().tx_permitted(NOW),
            "implicit-grab arm must permit TX exactly like the pre-controller path"
        );
        assert_eq!(
            ctx.controller.as_deref(),
            Some(CLIENT_KEY_ID),
            "the first control-mutating action implicitly grabs control"
        );
    }

    /// Rule 4: a control-mutating action from a peer that is NOT the controller
    /// is refused — it never reaches the rig, never implicitly grabs, and the
    /// refused peer receives an `Error` frame (targeted at just it). B is given
    /// qsy scope so the refusal is proven at the CONTROLLER layer, not the scope
    /// layer.
    #[tokio::test]
    async fn non_controller_qsy_refused_with_error_frame() {
        let mut ctx = ctx_with(true, true);
        ctx.peers.insert(
            PEER_B.to_string(),
            PeerCtx {
                hello_scopes: Some(vec!["qsy".to_string()]),
            },
        );
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        // A (CLIENT_KEY_ID) takes control.
        dispatch_action(
            ControlAction::TakeControl,
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(ctx.controller.as_deref(), Some(CLIENT_KEY_ID));
        // Qsy from B must be refused.
        let out = dispatch_action(
            ControlAction::Qsy {
                vfo: 0,
                frequency_hz: 14_074_000.0,
            },
            PEER_B,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(out.flow, Dispatch::Continue);
        assert!(
            hamlib_rx.try_recv().is_err(),
            "a non-controller Qsy must NOT reach the rig"
        );
        assert_eq!(
            ctx.controller.as_deref(),
            Some(CLIENT_KEY_ID),
            "a refused action must NOT implicitly grab control"
        );
        let err_to_b = out.sends.iter().any(|s| {
            matches!(
                (&s.to, &s.frame),
                (
                    SendTarget::One(p),
                    pancetta_protocol::ServerFrame::Event {
                        event: pancetta_protocol::ServerEvent::Error { .. }
                    }
                ) if p == PEER_B
            )
        });
        assert!(
            err_to_b,
            "the refused peer must receive an Error frame targeted at just it"
        );
    }

    /// Important (final whole-branch review): the ARM-path cross-peer-replay
    /// blocker. This pins the single most security-critical line on the branch —
    /// `verify_and_arm`'s `client_key_id != peer` check, which binds the grant's
    /// claimed identity to the ACTUAL demuxed sender of the frame, never to any
    /// session-global `ctx` state.
    ///
    /// Peer B is a fully-established, allow-listed peer with its OWN device key
    /// registered, but the `txArmGrant` it sends names peer A's `clientKeyId`
    /// (`CLIENT_KEY_ID`) — a grant that WOULD arm if A had sent it (identical to
    /// `arm_from_allowlisted_client_permits_tx`: valid token, valid clientSig,
    /// matching sessionId, consent ON). The ONLY thing standing between it and a
    /// live arm is the sender-binding check. It must be rejected: the arm never
    /// becomes armed, `tx_permitted()` stays false, and a `TxDenied` is audited.
    ///
    /// RED evidence: deleting the `client_key_id != peer` guard makes this arm
    /// succeed (verified by commenting the check out locally — `is_armed()` then
    /// flips true), so this test genuinely pins that line, not incidental setup.
    #[tokio::test]
    async fn arm_grant_naming_a_different_peer_than_the_sender_is_rejected() {
        // A (CLIENT_KEY_ID) is allow-listed with its device key (ctx_with). Add
        // B (PEER_B) as a SECOND allow-listed, established peer with its OWN
        // device key — a genuinely admitted second client, not a bare claim.
        let mut ctx = ctx_with(true, true);
        ctx.tx_allow_list
            .write()
            .unwrap()
            .insert(PEER_B.to_string());
        // PEER_B is a genuinely admitted AND TX-arm-eligible (non-delegated)
        // second client — the sender-binding check is the only thing tested
        // here, so B must not be excluded by the CQD-11 delegation gate too.
        ctx.tx_arm_allow_list
            .write()
            .unwrap()
            .insert(PEER_B.to_string());
        ctx.client_keys
            .write()
            .unwrap()
            .insert(PEER_B.to_string(), key(0x33).verifying_key());
        ctx.peers
            .insert(PEER_B.to_string(), PeerCtx { hello_scopes: None });
        // Consent ON: nothing but the sender-binding check gates this arm.
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();

        // `arm_action` names A (CLIENT_KEY_ID) and is signed by A's device key —
        // but the ACTUAL demuxed sender of this frame is B (the `peer` argument).
        let out = dispatch_action(
            arm_action("arm-jti-1"),
            PEER_B,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;

        assert_eq!(out.flow, Dispatch::Continue);
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "a grant naming a DIFFERENT peer than the demuxed sender must never arm"
        );
        assert!(
            !ctx.arm.lock().unwrap().tx_permitted(NOW),
            "tx must stay not-permitted after a rejected cross-peer arm grant"
        );

        // The denial is on the audit trail as a TxDenied ("arm rejected: ...").
        let log = std::fs::read_to_string(ctx.audit.path()).unwrap_or_default();
        let denied = log
            .lines()
            .filter_map(|l| serde_json::from_str::<AuditEvent>(l).ok())
            .any(|ev| ev.kind == AuditKind::TxDenied && ev.detail.contains("arm rejected"));
        assert!(
            denied,
            "a rejected cross-peer arm grant must record a TxDenied audit event"
        );
    }

    /// Rule 1: `takeControl` is a free grab from any established peer. When it
    /// displaces a DIFFERENT controller whose arm is live, that arm is disarmed
    /// FIRST (arms never transfer), THEN control moves — and a per-receiver
    /// `controlState` goes to every peer (new controller held=true, old
    /// held=false; transmit_armed=false for both).
    #[tokio::test]
    async fn take_control_disarms_previous_controllers_arm() {
        let mut ctx = ctx_with(true, true);
        ctx.peers
            .insert(PEER_B.to_string(), PeerCtx { hello_scopes: None });
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        // A arms (implicit grab → controller=A, armed).
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(ctx.arm.lock().unwrap().is_armed());
        assert_eq!(ctx.controller.as_deref(), Some(CLIENT_KEY_ID));
        // B takes control.
        let out = dispatch_action(
            ControlAction::TakeControl,
            PEER_B,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(out.flow, Dispatch::Continue);
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "takeControl must disarm the displaced controller's live arm first"
        );
        assert_eq!(ctx.controller.as_deref(), Some(PEER_B));
        // A per-receiver controlState to BOTH peers.
        let held = |peer: &str| {
            out.sends.iter().find_map(|s| match (&s.to, &s.frame) {
                (
                    SendTarget::One(p),
                    pancetta_protocol::ServerFrame::Event {
                        event:
                            pancetta_protocol::ServerEvent::ControlState {
                                control_held_by_me,
                                transmit_armed,
                            },
                    },
                ) if p == peer => Some((*control_held_by_me, *transmit_armed)),
                _ => None,
            })
        };
        assert_eq!(
            held(PEER_B),
            Some((true, false)),
            "new controller B: holds control, not armed"
        );
        assert_eq!(
            held(CLIENT_KEY_ID),
            Some((false, false)),
            "displaced controller A: no longer holds control, not armed"
        );
    }

    /// Rule 5: `Disarm` is accepted from ANY established peer — even one that is
    /// not the controller. Fail-safe TX-OFF beats exclusivity.
    #[tokio::test]
    async fn disarm_accepted_from_non_controller() {
        let mut ctx = ctx_with(true, true);
        ctx.peers
            .insert(PEER_B.to_string(), PeerCtx { hello_scopes: None });
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(ctx.arm.lock().unwrap().is_armed());
        assert_eq!(ctx.controller.as_deref(), Some(CLIENT_KEY_ID));
        // Disarm from B (NOT the controller) is accepted.
        let out = dispatch_action(
            ControlAction::Disarm {
                arm_jti: String::new(),
            },
            PEER_B,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(out.flow, Dispatch::Continue);
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "Disarm from any established peer must disarm (rule 5, fail-safe)"
        );
    }

    /// Rule 2: `releaseControl` from the current controller disarms (if armed)
    /// and clears the controller; from anyone else it is a silent no-op.
    #[tokio::test]
    async fn release_control_clears_and_disarms() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(ctx.arm.lock().unwrap().is_armed());
        assert_eq!(ctx.controller.as_deref(), Some(CLIENT_KEY_ID));
        // The controller releases → disarm + clear controller.
        let out = dispatch_action(
            ControlAction::ReleaseControl,
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert_eq!(out.flow, Dispatch::Continue);
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "releaseControl from the controller must disarm"
        );
        assert!(
            ctx.controller.is_none(),
            "releaseControl from the controller must clear the controller"
        );
        // releaseControl from a non-controller (here: nobody holds control) is a
        // silent no-op — no frames, no state change.
        let out2 = dispatch_action(
            ControlAction::ReleaseControl,
            PEER_B,
            &mut ctx,
            &bus,
            SESSION_ID,
            NOW,
        )
        .await;
        assert!(
            out2.sends.is_empty(),
            "releaseControl from a non-controller emits nothing"
        );
        assert!(ctx.controller.is_none());
    }

    /// Rule 6: in `run_one_session`, a controller leaving disarms + clears the
    /// controller; a non-controller (listener) leaving disarms nothing. Driven
    /// through the real `MultiPeerSession` with scripted relay frames. The arm
    /// is pre-armed (with the controller implicitly grabbed) before the run; the
    /// in-session `presence:down` is what exercises the `PeerDown` arm.
    #[tokio::test]
    async fn controller_peer_down_disarms_listener_down_does_not() {
        // --- Scenario 1: the CONTROLLER (A = CLIENT_KEY_ID) goes down. --------
        {
            let identity = AgentIdentity::generate();
            let mut ctx = ctx_with(true, true);
            with_consent(&ctx, true);
            let bus = MessageBus::new(64).unwrap();
            // Pre-arm: implicit grab makes CLIENT_KEY_ID the controller.
            dispatch_action(
                arm_action("arm-jti-1"),
                CLIENT_KEY_ID,
                &mut ctx,
                &bus,
                SESSION_ID,
                NOW,
            )
            .await;
            assert!(ctx.arm.lock().unwrap().is_armed());
            assert_eq!(ctx.controller.as_deref(), Some(CLIENT_KEY_ID));

            let ws = scripted_ws_for_peer_down(&identity, CLIENT_KEY_ID);
            run_one_session(ws, &identity, &mut ctx, &bus, &mut None).await;

            assert!(
                !ctx.arm.lock().unwrap().is_armed(),
                "the controller leaving must disarm"
            );
            assert!(
                ctx.controller.is_none(),
                "the controller leaving must clear the controller"
            );
        }

        // --- Scenario 2: a LISTENER (B) goes down; A stays controller+armed. --
        {
            let identity = AgentIdentity::generate();
            let mut ctx = ctx_with(true, true);
            // B must be allow-listed so MultiPeerSession admits it.
            ctx.tx_allow_list
                .write()
                .unwrap()
                .insert(PEER_B.to_string());
            with_consent(&ctx, true);
            let bus = MessageBus::new(64).unwrap();
            dispatch_action(
                arm_action("arm-jti-1"),
                CLIENT_KEY_ID,
                &mut ctx,
                &bus,
                SESSION_ID,
                NOW,
            )
            .await;
            assert!(ctx.arm.lock().unwrap().is_armed());
            assert_eq!(ctx.controller.as_deref(), Some(CLIENT_KEY_ID));

            let ws = scripted_ws_for_peer_down(&identity, PEER_B);
            run_one_session(ws, &identity, &mut ctx, &bus, &mut None).await;

            assert!(
                ctx.arm.lock().unwrap().is_armed(),
                "a non-controller (listener) leaving must NOT disarm"
            );
            assert_eq!(
                ctx.controller.as_deref(),
                Some(CLIENT_KEY_ID),
                "a listener leaving must NOT change the controller"
            );
        }
    }

    /// Build a scripted `MockWs` that establishes `connecting_client` over a real
    /// Noise IK handshake against `identity`, then sends a `presence:down` for
    /// that peer (draining the inbound queue ends the session). Used to drive
    /// `run_one_session`'s `PeerDown` arm.
    fn scripted_ws_for_peer_down(identity: &AgentIdentity, connecting_client: &str) -> MockWs {
        let agent_static_pub = identity.agreement_public_raw();
        let client_kp = {
            let params: snow::params::NoiseParams =
                "Noise_IK_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
            snow::Builder::new(params).generate_keypair().unwrap()
        };
        let mut initiator = TestInitiator::new(&client_kp.private, &agent_static_pub);
        let msg1 = initiator.write_msg1(b"");

        let hello = RelayFrame::Hello {
            challenge: b64url(&[7u8; 32]),
        }
        .to_json()
        .unwrap();
        let ready = RelayFrame::Ready {
            key_id: identity.key_id(),
            peer_present: true,
        }
        .to_json()
        .unwrap();
        let env_msg1 = RelayFrame::Env {
            dst: identity.key_id(),
            payload: b64url(&msg1),
            src: Some(connecting_client.to_string()),
        }
        .to_json()
        .unwrap();
        let presence_down = RelayFrame::Presence {
            peer: connecting_client.to_string(),
            state: "down".to_string(),
        }
        .to_json()
        .unwrap();

        MockWs::new(
            vec![hello, ready, env_msg1, presence_down],
            Arc::new(Mutex::new(Vec::new())),
        )
    }

    // ========================================================================
    // FULL end-to-end integration proof (the security milestone).
    //
    // A scripted mock relay + mock client drive the REAL `AgentSession` through
    // relay auth → Noise IK handshake → an encrypted `Arm` control frame, which
    // `run_one_session` decrypts, maps, and dispatches into the shared
    // `remote_tx_arm`. We assert the arm becomes tx-permitted (and the negative
    // cases: consent-off never permits; un-allow-listed rejected).
    // ========================================================================

    use pancetta_agent::relay::{parse_frame, RelayError, RelayFrame, WsConn};

    /// A scripted mock WS: a shared queue of inbound frames (pushable) + captured
    /// outbound frames.
    #[derive(Clone)]
    struct MockWs {
        inbound: Arc<Mutex<std::collections::VecDeque<String>>>,
        outbound: Arc<Mutex<Vec<String>>>,
    }

    impl MockWs {
        fn new(inbound: Vec<String>, outbound: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                inbound: Arc::new(Mutex::new(inbound.into_iter().collect())),
                outbound,
            }
        }
        fn push_inbound(&self, s: String) {
            self.inbound.lock().unwrap().push_back(s);
        }
    }

    impl WsConn for MockWs {
        fn send_text(&mut self, s: String) -> Result<(), RelayError> {
            self.outbound.lock().unwrap().push(s);
            Ok(())
        }
        fn recv_text(&mut self) -> Result<Option<String>, RelayError> {
            Ok(self.inbound.lock().unwrap().pop_front())
        }
    }

    /// Wraps a `MockWs`, running a one-shot side effect right before its
    /// `recv_text_within`-triggering `recv_text` call number `trigger_after`
    /// returns. Lets a test deterministically place "a concurrent event
    /// happened here" at an EXACT point in the tick sequence — without
    /// needing genuine thread concurrency, which `run_one_session`'s
    /// MockWs-driven tick loop has no reliable yield point for (it is
    /// entirely synchronous except inside `dispatch_action`'s own internal
    /// awaits). Used to simulate "a concurrent `poll_authorizations_loop`
    /// cycle disarmed the live arm and set `revocation_pending` between two
    /// specific ticks."
    struct TriggerAfterWs {
        inner: MockWs,
        trigger_after: usize,
        calls: usize,
        on_trigger: Option<Box<dyn FnOnce() + Send>>,
    }

    impl TriggerAfterWs {
        fn new(
            inner: MockWs,
            trigger_after: usize,
            on_trigger: impl FnOnce() + Send + 'static,
        ) -> Self {
            Self {
                inner,
                trigger_after,
                calls: 0,
                on_trigger: Some(Box::new(on_trigger)),
            }
        }
    }

    impl WsConn for TriggerAfterWs {
        fn send_text(&mut self, s: String) -> Result<(), RelayError> {
            self.inner.send_text(s)
        }
        fn recv_text(&mut self) -> Result<Option<String>, RelayError> {
            self.calls += 1;
            if self.calls == self.trigger_after {
                if let Some(f) = self.on_trigger.take() {
                    f();
                }
            }
            self.inner.recv_text()
        }
    }

    /// A test-only Noise IK initiator (the client side), mirroring session.rs.
    struct TestInitiator {
        inner: snow::HandshakeState,
    }
    impl TestInitiator {
        fn new(local_priv: &[u8], remote_pub: &[u8]) -> Self {
            let params: snow::params::NoiseParams =
                "Noise_IK_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
            let inner = snow::Builder::new(params)
                .local_private_key(local_priv)
                .unwrap()
                .remote_public_key(remote_pub)
                .unwrap()
                .build_initiator()
                .unwrap();
            Self { inner }
        }
        fn write_msg1(&mut self, payload: &[u8]) -> Vec<u8> {
            let mut buf = vec![0u8; 65535];
            let n = self.inner.write_message(payload, &mut buf).unwrap();
            buf.truncate(n);
            buf
        }
        fn read_msg2(&mut self, msg2: &[u8]) {
            let mut buf = vec![0u8; 65535];
            self.inner.read_message(msg2, &mut buf).unwrap();
        }
        fn into_transport(self) -> snow::TransportState {
            self.inner.into_transport_mode().unwrap()
        }
        fn handshake_hash(&self) -> Vec<u8> {
            self.inner.get_handshake_hash().to_vec()
        }
    }

    /// Mint a valid TX-enabled capabilityToken whose `aud` is `agent_key_id`.
    fn token_for_agent(agent_key_id: &str) -> String {
        let header = json!({ "alg": "EdDSA", "kid": IDP_KID, "typ": "JWT" });
        let payload = json!({
            "iss": "cqdx", "operatorCallsign": OPERATOR,
            "aud": agent_key_id, "clientKeyId": CLIENT_KEY_ID,
            "scopes": ["status", "tx"],
            "iat": NOW / 1000 - 10, "exp": NOW / 1000 + 600,
            "txEnabledUntil": NOW / 1000 + 600, "jti": "cap-jti-1"
        });
        mint_jws(&header, &payload, &idp_key())
    }

    /// A full, frozen `txArm` INNER control frame whose `aud` is `agent_key_id`:
    /// `{type:"txArm", capabilityToken, grant}` — the token is a SIBLING of the
    /// client-signed grant (NOT inside it, per the frozen e2e-auth.v1 $defs).
    fn tx_arm_frame_for_agent(agent_key_id: &str, session_id: &str) -> Value {
        let mut grant = json!({
            "aud": agent_key_id,
            "clientKeyId": CLIENT_KEY_ID,
            "sessionId": session_id,
            "capabilityJti": "cap-jti-1",
            "operatorCallsign": OPERATOR,
            "armedAt": NOW,
            "armedUntil": NOW + 300_000,
            "heartbeatIntervalSec": 10,
            "jti": "arm-jti-1"
        })
        .as_object()
        .unwrap()
        .clone();
        let canon = canonical_bytes(&grant);
        let sig = client_key().sign(&domain_separated(&canon));
        grant.insert("clientSig".to_string(), json!(b64url(&sig.to_bytes())));
        json!({
            "type": "txArm",
            "capabilityToken": token_for_agent(agent_key_id),
            "grant": Value::Object(grant),
        })
    }

    /// The full end-to-end proof: a scripted relay + client drive the real
    /// AgentSession through auth + Noise + an encrypted Arm frame, and the shared
    /// arm becomes tx-permitted.
    #[tokio::test]
    async fn e2e_arm_over_noise_permits_remote_tx() {
        let identity = AgentIdentity::generate();
        let agent_kid = identity.key_id();
        let client_kid = CLIENT_KEY_ID.to_string();

        // Client-side Noise initiator.
        let agent_static_pub = identity.agreement_public_raw();
        let client_kp = {
            let params: snow::params::NoiseParams =
                "Noise_IK_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
            snow::Builder::new(params).generate_keypair().unwrap()
        };
        let mut initiator = TestInitiator::new(&client_kp.private, &agent_static_pub);
        let msg1 = initiator.write_msg1(b"");

        let hello = RelayFrame::Hello {
            challenge: b64url(&[3u8; 32]),
        }
        .to_json()
        .unwrap();
        let ready = RelayFrame::Ready {
            key_id: agent_kid.clone(),
            peer_present: true,
        }
        .to_json()
        .unwrap();
        let env_msg1 = RelayFrame::Env {
            dst: agent_kid.clone(),
            payload: b64url(&msg1),
            src: Some(client_kid.clone()),
        }
        .to_json()
        .unwrap();

        // Drive the session far enough to emit msg2, so we can complete the
        // client transport and then encrypt the Arm frame.
        let outbound = Arc::new(Mutex::new(Vec::new()));
        let ws = MockWs::new(vec![hello, ready, env_msg1], outbound.clone());
        let ws_handle = ws.clone(); // shares the inbound queue for later push.
        let mut sess = AgentSession::new(ws, &identity, client_kid.clone());
        sess.authenticate().unwrap();
        sess.process_next().unwrap(); // ready
        sess.process_next().unwrap(); // env(msg1) → emits msg2, transport up
        assert!(sess.is_transport_established());

        // Complete the client transport from the emitted msg2 (outbound[1]).
        let out = outbound.lock().unwrap().clone();
        let msg2 = match parse_frame(&out[1]).unwrap() {
            RelayFrame::Env { payload, .. } => base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&payload)
                .unwrap(),
            _ => panic!("expected env(msg2)"),
        };
        initiator.read_msg2(&msg2);
        // Q-0022: capture the client's channel-binding session_id BEFORE
        // `into_transport` consumes the handshake — this is the same value
        // the agent's `sess.session_id()` derives from its side of the SAME
        // completed handshake, proving the real chain end-to-end (not two
        // sides independently agreeing on a hardcoded test constant).
        let client_session_id = b64url(&initiator.handshake_hash());
        assert_eq!(
            sess.session_id(),
            Some(client_session_id.as_str()),
            "agent and client must derive the identical channel-binding session_id"
        );
        let mut client_transport = initiator.into_transport();

        // Encrypt the real txArm control frame (token+grant siblings) and hand
        // it to the session as an env.
        let arm_frame = tx_arm_frame_for_agent(&agent_kid, &client_session_id);
        let plaintext = serde_json::to_vec(&arm_frame).unwrap();
        let mut ct = vec![0u8; plaintext.len() + 16];
        let n = client_transport.write_message(&plaintext, &mut ct).unwrap();
        ct.truncate(n);
        let arm_env = RelayFrame::Env {
            dst: agent_kid.clone(),
            payload: b64url(&ct),
            src: Some(client_kid.clone()),
        }
        .to_json()
        .unwrap();

        // Feed the encrypted Arm env into the session's inbound queue and let the
        // REAL session decrypt it → map → dispatch → arm.
        ws_handle.push_inbound(arm_env);
        let decrypted = sess
            .process_next()
            .expect("decrypt arm env")
            .expect("arm plaintext");
        let action = map_client_frame(&decrypted).unwrap();
        assert!(matches!(action, ControlAction::Arm { .. }));

        // Use the agent's OWN session_id() (not the hardcoded SESSION_ID test
        // constant other tests use) — this is what makes this test an actual
        // end-to-end proof of Q-0022's wiring, not just two hardcoded strings
        // matching each other.
        let agent_session_id = sess.session_id().expect("transport established");

        let mut ctx = ctx_with_agent(&agent_kid, true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        let d = dispatch_action(action, CLIENT_KEY_ID, &mut ctx, &bus, agent_session_id, NOW).await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            ctx.arm.lock().unwrap().tx_permitted(NOW),
            "end-to-end: a verified Arm over Noise must permit remote TX"
        );

        // Negative: the same flow with consent OFF never permits.
        let mut ctx_off = ctx_with_agent(&agent_kid, true, true);
        // (consent left OFF)
        let action2 = map_client_frame(&decrypted).unwrap();
        dispatch_action(
            action2,
            CLIENT_KEY_ID,
            &mut ctx_off,
            &bus,
            agent_session_id,
            NOW,
        )
        .await;
        // jti replay guard is per-ctx (fresh seen set), so this arms the state
        // machine but consent-off denies at the gate.
        assert!(
            !ctx_off.arm.lock().unwrap().tx_permitted(NOW),
            "consent OFF must deny even after a verified Arm over Noise"
        );
    }

    /// Drive a real Noise handshake (via `run_one_session`'s `MultiPeerSession`,
    /// not `AgentSession` directly) from `connecting_client`, against a station
    /// whose `tx_allow_list` is `allow`. Returns the resulting `ArmContext` so
    /// the caller can inspect `ctx.peers`.
    async fn run_session_with_connecting_peer(
        identity: &AgentIdentity,
        connecting_client: &str,
        allow: HashSet<String>,
    ) -> ArmContext {
        let agent_static_pub = identity.agreement_public_raw();
        let client_kp = {
            let params: snow::params::NoiseParams =
                "Noise_IK_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
            snow::Builder::new(params).generate_keypair().unwrap()
        };
        let mut initiator = TestInitiator::new(&client_kp.private, &agent_static_pub);
        let msg1 = initiator.write_msg1(b"");

        let hello = RelayFrame::Hello {
            challenge: b64url(&[5u8; 32]),
        }
        .to_json()
        .unwrap();
        let ready = RelayFrame::Ready {
            key_id: identity.key_id(),
            peer_present: true,
        }
        .to_json()
        .unwrap();
        let env_msg1 = RelayFrame::Env {
            dst: identity.key_id(),
            payload: b64url(&msg1),
            src: Some(connecting_client.to_string()),
        }
        .to_json()
        .unwrap();

        let outbound = Arc::new(Mutex::new(Vec::new()));
        let ws = MockWs::new(vec![hello, ready, env_msg1], outbound);

        let mut client_keys = std::collections::HashMap::new();
        if allow.contains(connecting_client) {
            client_keys.insert(connecting_client.to_string(), client_key().verifying_key());
        }
        // This helper exercises connection admission only (never arming), so
        // the TX-arm-eligible set mirrors the admission set — no test using
        // it depends on the two diverging.
        let tx_arm_allow = allow.clone();
        let mut ctx = ArmContext {
            arm: Arc::new(Mutex::new(ArmState::new())),
            verifier: CapabilityVerifier {
                agent_key_id: identity.key_id(),
                pinned_idp_keys: vec![IdpKey {
                    kid: IDP_KID.to_string(),
                    public_key: idp_key().verifying_key().to_bytes(),
                }],
            },
            client_keys: Arc::new(std::sync::RwLock::new(client_keys)),
            tx_allow_list: Arc::new(std::sync::RwLock::new(allow)),
            tx_arm_allow_list: Arc::new(std::sync::RwLock::new(tx_arm_allow)),
            revoked_jtis: HashSet::new(),
            seen_jtis: HashSet::new(),
            audit: AuditLog::new(audit_tmp()),
            peers: std::collections::HashMap::new(),
            controller: None,
            revocation_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let bus = MessageBus::new(64).unwrap();
        run_one_session(ws, identity, &mut ctx, &bus, &mut None).await;
        ctx
    }

    /// Dynamic client selection (Q-0043 quick fix): the peer that actually
    /// connects is learned and vetted — it does NOT need to be the
    /// lexicographically-first entry in `tx_allow_list` (the old, now-removed
    /// pre-pick-at-startup behavior).
    #[tokio::test]
    async fn dynamic_selection_accepts_whichever_allowlisted_peer_connects() {
        let identity = AgentIdentity::generate();
        let connecting_client = "zzzLastAlphabetically".to_string();
        let mut allow = HashSet::new();
        allow.insert("aaaFirstAlphabetically".to_string());
        allow.insert(connecting_client.clone());

        let ctx = run_session_with_connecting_peer(&identity, &connecting_client, allow).await;

        assert!(
            ctx.peers.contains_key(&connecting_client),
            "whichever allow-listed client actually connects must be admitted, \
             regardless of allow-list ordering"
        );
    }

    /// A peer the relay admits (it completed the Noise handshake — the relay
    /// authenticated its `src`) but that is NOT in the station-local
    /// `tx_allow_list` must be refused before any scope is granted. The
    /// station-local list, not relay admission, is the authoritative gate.
    ///
    /// Task 4: with `MultiPeerSession`, a refused peer is isolated to itself —
    /// `PeerRefused` never tears down the whole session (unlike the old
    /// single-peer `AgentSession`, where refusing the one connected peer ended
    /// the session outright). So the assertion here is no longer "the session
    /// tore down" but "no `PeerCtx` was ever created for the refused peer / it
    /// was served no scope" — `MAX_PEERS` admission runs BEFORE any handshake
    /// state is allocated, so a refused peer never gets a `PeerEstablished`.
    #[tokio::test]
    async fn relay_admitted_peer_not_in_allow_list_is_refused() {
        let identity = AgentIdentity::generate();
        let connecting_client = "notAllowlistedClientKeyId".to_string();
        let mut allow = HashSet::new();
        allow.insert("someOtherAllowlistedClient".to_string());

        let ctx = run_session_with_connecting_peer(&identity, &connecting_client, allow).await;

        assert!(
            !ctx.peers.contains_key(&connecting_client),
            "a relay-admitted peer absent from tx_allow_list must never get a PeerCtx / be vetted in"
        );
        assert!(
            ctx.peers.is_empty(),
            "the refused peer must be served no scope at all — no PeerCtx anywhere in this session"
        );
    }

    /// A fresh `ArmContext` admitting exactly `allow` over a fresh
    /// `AgentIdentity`. No fixed IDP/agent-key constants tied to it (unlike
    /// `ctx_with`) — used by the Task 6 read-stream tests, which only care
    /// about peer establishment + the display feed, never about arming.
    fn fresh_ctx(agent_kid: &str, allow: HashSet<String>) -> ArmContext {
        // Read-stream/peer-establishment tests only — arming is never
        // exercised through this helper, so the TX-arm-eligible set mirrors
        // the admission set.
        let tx_arm_allow = allow.clone();
        ArmContext {
            arm: Arc::new(Mutex::new(ArmState::new())),
            verifier: CapabilityVerifier {
                agent_key_id: agent_kid.to_string(),
                pinned_idp_keys: vec![IdpKey {
                    kid: IDP_KID.to_string(),
                    public_key: idp_key().verifying_key().to_bytes(),
                }],
            },
            client_keys: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            tx_allow_list: Arc::new(std::sync::RwLock::new(allow)),
            tx_arm_allow_list: Arc::new(std::sync::RwLock::new(tx_arm_allow)),
            revoked_jtis: HashSet::new(),
            seen_jtis: HashSet::new(),
            audit: AuditLog::new(audit_tmp()),
            peers: std::collections::HashMap::new(),
            controller: None,
            revocation_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// A client-side Noise IK initiator with a freshly generated keypair,
    /// against `agent_pub`. Mirrors the inline pattern used throughout this
    /// module's other scripted tests.
    fn fresh_initiator(agent_pub: &[u8]) -> TestInitiator {
        let params: snow::params::NoiseParams =
            "Noise_IK_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
        let kp = snow::Builder::new(params).generate_keypair().unwrap();
        TestInitiator::new(&kp.private, agent_pub)
    }

    /// Task 6: a `ServerEvent` broadcast into the shared display feed BEFORE
    /// the session runs must reach EVERY established peer, each decrypted
    /// under its OWN independent Noise transport — proven at the
    /// `run_one_session` level (not just `MultiPeerSession::broadcast`'s own
    /// unit coverage in `pancetta-agent`).
    ///
    /// No `ready`/heartbeat frame is scripted: peer establishment does not
    /// require the relay's `ready` first (mirrors `process_env`, which has no
    /// `admitted` gate), and inserting a `ready` step would trigger an early
    /// `Poll::Idle` — and therefore an early `drain_read_stream` — BEFORE any
    /// peer is established, permanently consuming the one buffered event
    /// with zero recipients (draining is a plain `try_recv`, oblivious to
    /// whether anyone is listening). The trailing `presence:up` frame is inert
    /// on its own (informational only — `state != "down"` never touches
    /// `ctx.peers`) and exists solely to produce the `Poll::Idle` tick that
    /// exercises the drain AFTER both peers are established.
    #[tokio::test]
    async fn read_stream_events_reach_every_established_peer() {
        let identity = AgentIdentity::generate();
        let agent_kid = identity.key_id();
        let agent_pub = identity.agreement_public_raw();
        const PEER_A: &str = CLIENT_KEY_ID;

        let mut allow = HashSet::new();
        allow.insert(PEER_A.to_string());
        allow.insert(PEER_B.to_string());
        let mut ctx = fresh_ctx(&agent_kid, allow);
        let bus = MessageBus::new(64).unwrap();

        let mut init_a = fresh_initiator(&agent_pub);
        let mut init_b = fresh_initiator(&agent_pub);
        let msg1_a = init_a.write_msg1(b"");
        let msg1_b = init_b.write_msg1(b"");

        let hello = RelayFrame::Hello {
            challenge: b64url(&[11u8; 32]),
        }
        .to_json()
        .unwrap();
        let env_a = RelayFrame::Env {
            dst: agent_kid.clone(),
            payload: b64url(&msg1_a),
            src: Some(PEER_A.to_string()),
        }
        .to_json()
        .unwrap();
        let env_b = RelayFrame::Env {
            dst: agent_kid.clone(),
            payload: b64url(&msg1_b),
            src: Some(PEER_B.to_string()),
        }
        .to_json()
        .unwrap();
        // A benign relay frame that yields Poll::Idle without touching
        // ctx.peers (see the doc comment above) — the tick that lets
        // `drain_read_stream` run with both peers already established.
        let idle_tick = RelayFrame::Presence {
            peer: "irrelevant-peer".to_string(),
            state: "up".to_string(),
        }
        .to_json()
        .unwrap();

        let outbound = Arc::new(Mutex::new(Vec::new()));
        let ws = MockWs::new(vec![hello, env_a, env_b, idle_tick], outbound.clone());

        // Subscribe BEFORE sending so this receiver is guaranteed the event.
        let (evt_tx, _keepalive) =
            tokio::sync::broadcast::channel::<pancetta_protocol::ServerEvent>(16);
        let mut events = Some(evt_tx.subscribe());
        evt_tx
            .send(pancetta_protocol::ServerEvent::TxStatus { active: true })
            .unwrap();

        run_one_session(ws, &identity, &mut ctx, &bus, &mut events).await;

        assert!(ctx.peers.contains_key(PEER_A), "peer A must be established");
        assert!(ctx.peers.contains_key(PEER_B), "peer B must be established");

        // Collect, in wire order, every env addressed to each peer: msg2,
        // then the PeerEstablished greet, then (from the drain) the event.
        let out = outbound.lock().unwrap().clone();
        let envs_for = |peer: &str| -> Vec<Vec<u8>> {
            out.iter()
                .filter_map(|s| match parse_frame(s).unwrap() {
                    RelayFrame::Env { dst, payload, .. } if dst == peer => Some(
                        base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(&payload)
                            .unwrap(),
                    ),
                    _ => None,
                })
                .collect()
        };
        let envs_a = envs_for(PEER_A);
        let envs_b = envs_for(PEER_B);
        assert_eq!(
            envs_a.len(),
            3,
            "A: msg2 + PeerEstablished greet + the drained event"
        );
        assert_eq!(
            envs_b.len(),
            3,
            "B: msg2 + PeerEstablished greet + the drained event"
        );

        // Complete each initiator's client transport from its own msg2 (the
        // first env addressed to it), then decrypt the rest IN ORDER (Noise
        // transport nonces are sequential — skipping one desyncs the rest).
        init_a.read_msg2(&envs_a[0]);
        init_b.read_msg2(&envs_b[0]);
        let mut ta = init_a.into_transport();
        let mut tb = init_b.into_transport();

        let decrypt_all = |t: &mut snow::TransportState, cts: &[Vec<u8>]| -> Vec<Vec<u8>> {
            cts.iter()
                .map(|ct| {
                    let mut buf = vec![0u8; ct.len().max(1)];
                    let n = t.read_message(ct, &mut buf).unwrap();
                    buf.truncate(n);
                    buf
                })
                .collect()
        };
        let plain_a = decrypt_all(&mut ta, &envs_a[1..]);
        let plain_b = decrypt_all(&mut tb, &envs_b[1..]);

        // The LAST decrypted frame for each peer must be our event, matching
        // the wire contract exactly (parsed JSON, not string equality).
        for plaintext in [plain_a.last().unwrap(), plain_b.last().unwrap()] {
            let v: serde_json::Value = serde_json::from_slice(plaintext).unwrap();
            assert_eq!(v["frame"], "event");
            assert_eq!(v["event"]["event"], "txStatus");
            assert_eq!(v["event"]["active"], true);
        }
    }

    /// Task 6: with no display feed subscribed (`events: None` — the inert
    /// path when neither the localhost gateway nor `station_agent_active`
    /// started one), `drain_read_stream` must be a strict no-op: a
    /// `Poll::Idle` tick after a peer establishes sends nothing beyond the
    /// ordinary handshake + greet traffic that peer establishment always
    /// produces.
    #[tokio::test]
    async fn read_stream_absent_feed_is_inert() {
        let identity = AgentIdentity::generate();
        let agent_kid = identity.key_id();
        let agent_pub = identity.agreement_public_raw();

        let mut allow = HashSet::new();
        allow.insert(CLIENT_KEY_ID.to_string());
        let mut ctx = fresh_ctx(&agent_kid, allow);
        let bus = MessageBus::new(64).unwrap();

        let mut init = fresh_initiator(&agent_pub);
        let msg1 = init.write_msg1(b"");

        let hello = RelayFrame::Hello {
            challenge: b64url(&[12u8; 32]),
        }
        .to_json()
        .unwrap();
        let env = RelayFrame::Env {
            dst: agent_kid.clone(),
            payload: b64url(&msg1),
            src: Some(CLIENT_KEY_ID.to_string()),
        }
        .to_json()
        .unwrap();
        let idle_tick = RelayFrame::Presence {
            peer: "irrelevant-peer".to_string(),
            state: "up".to_string(),
        }
        .to_json()
        .unwrap();

        let outbound = Arc::new(Mutex::new(Vec::new()));
        let ws = MockWs::new(vec![hello, env, idle_tick], outbound.clone());

        let mut events: Option<tokio::sync::broadcast::Receiver<pancetta_protocol::ServerEvent>> =
            None;
        run_one_session(ws, &identity, &mut ctx, &bus, &mut events).await;

        assert!(
            ctx.peers.contains_key(CLIENT_KEY_ID),
            "peer must still establish normally"
        );

        // auth (relay leg) + msg2 (handshake) + the PeerEstablished greet —
        // and NOTHING else. The trailing Idle tick (from `presence:up`) must
        // not have added a single outbound frame.
        let out = outbound.lock().unwrap().clone();
        assert_eq!(
            out.len(),
            3,
            "an absent feed must add zero outbound frames beyond ordinary handshake traffic: {out:?}"
        );
    }

    /// Build an ArmContext whose verifier expects `agent_kid` as the aud.
    fn ctx_with_agent(agent_kid: &str, allow_client: bool, have_device_key: bool) -> ArmContext {
        let mut allow = HashSet::new();
        if allow_client {
            allow.insert(CLIENT_KEY_ID.to_string());
        }
        let mut client_keys = std::collections::HashMap::new();
        if have_device_key {
            client_keys.insert(CLIENT_KEY_ID.to_string(), client_key().verifying_key());
        }
        // `allow_client` means an OWNER edge here too — see `ctx_with`.
        let tx_arm_allow = allow.clone();
        ArmContext {
            arm: Arc::new(Mutex::new(ArmState::new())),
            verifier: CapabilityVerifier {
                agent_key_id: agent_kid.to_string(),
                pinned_idp_keys: vec![IdpKey {
                    kid: IDP_KID.to_string(),
                    public_key: idp_key().verifying_key().to_bytes(),
                }],
            },
            client_keys: Arc::new(std::sync::RwLock::new(client_keys)),
            tx_allow_list: Arc::new(std::sync::RwLock::new(allow)),
            tx_arm_allow_list: Arc::new(std::sync::RwLock::new(tx_arm_allow)),
            revoked_jtis: HashSet::new(),
            seen_jtis: HashSet::new(),
            audit: AuditLog::new(audit_tmp()),
            peers: peers_with(CLIENT_KEY_ID, None),
            controller: None,
            revocation_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[test]
    fn authorizations_filter_matches_only_this_agents_edges() {
        let edges = vec![
            pancetta_cqdx::AuthorizationEdge {
                id: "1".to_string(),
                agent_key_id: "agent_this".to_string(),
                client_key_id: "client_a".to_string(),
                scopes: vec!["status".to_string()],
                created_at: "2026-07-20T00:00:00Z".to_string(),
                role: None,
                delegated_by: None,
            },
            pancetta_cqdx::AuthorizationEdge {
                id: "2".to_string(),
                agent_key_id: "agent_other".to_string(),
                client_key_id: "client_b".to_string(),
                scopes: vec!["status".to_string()],
                created_at: "2026-07-20T00:00:00Z".to_string(),
                role: None,
                delegated_by: None,
            },
        ];
        let filtered: HashSet<String> = edges
            .iter()
            .filter(|e| e.agent_key_id == "agent_this")
            .map(|e| e.client_key_id.clone())
            .collect();
        assert_eq!(filtered, HashSet::from(["client_a".to_string()]));
    }

    /// CQD-11 / dispensa Q-0052: the TX-arm-eligible filter must admit an
    /// owner edge whose `delegated_by` is `None` — regardless of whether that
    /// `None` came from a wholly ABSENT `delegatedBy` field or an explicit
    /// JSON `null` (both deserialize to `None` via `AuthorizationEdge`'s
    /// `#[serde(default)]` — see `pancetta-cqdx`'s own coverage of that) — and
    /// must EXCLUDE a delegated edge (`delegated_by: Some(..)`), while the
    /// connection-admission filter (`agentKeyId` only) keeps ALL THREE.
    #[test]
    fn tx_arm_filter_excludes_delegated_edges_but_admission_filter_keeps_them() {
        let edges = vec![
            // Owner edge, delegatedBy field entirely absent from the wire —
            // deserializes to `None` (mirrors `pancetta-cqdx`'s absent-field
            // fixture).
            pancetta_cqdx::AuthorizationEdge {
                id: "1".to_string(),
                agent_key_id: "agent_this".to_string(),
                client_key_id: "client_owner_absent".to_string(),
                scopes: vec!["status".to_string(), "qsy".to_string(), "tx".to_string()],
                created_at: "2026-07-20T00:00:00Z".to_string(),
                role: None,
                delegated_by: None,
            },
            // Delegated status/qsy guest edge — must be admitted but NOT
            // TX-arm-eligible.
            pancetta_cqdx::AuthorizationEdge {
                id: "2".to_string(),
                agent_key_id: "agent_this".to_string(),
                client_key_id: "client_delegate".to_string(),
                scopes: vec!["status".to_string(), "qsy".to_string()],
                created_at: "2026-07-20T00:00:00Z".to_string(),
                role: Some("owner".to_string()),
                delegated_by: Some("user_owner_123".to_string()),
            },
            // A different agent's edge entirely — excluded from both sets by
            // the pre-existing `agentKeyId` filter.
            pancetta_cqdx::AuthorizationEdge {
                id: "3".to_string(),
                agent_key_id: "agent_other".to_string(),
                client_key_id: "client_unrelated".to_string(),
                scopes: vec!["status".to_string()],
                created_at: "2026-07-20T00:00:00Z".to_string(),
                role: None,
                delegated_by: None,
            },
        ];

        let admission: HashSet<String> = edges
            .iter()
            .filter(|e| e.agent_key_id == "agent_this")
            .map(|e| e.client_key_id.clone())
            .collect();
        let tx_arm_eligible: HashSet<String> = edges
            .iter()
            .filter(|e| e.agent_key_id == "agent_this" && e.delegated_by.is_none())
            .map(|e| e.client_key_id.clone())
            .collect();

        assert_eq!(
            admission,
            HashSet::from([
                "client_owner_absent".to_string(),
                "client_delegate".to_string(),
            ]),
            "connection admission must be scope-blind and include the delegated guest"
        );
        assert_eq!(
            tx_arm_eligible,
            HashSet::from(["client_owner_absent".to_string()]),
            "TX-arm eligibility must exclude the delegated guest"
        );
    }

    #[tokio::test]
    async fn poll_authorizations_loop_keeps_last_known_good_on_failure() {
        // A cqdx client pointed at a URL with nothing listening fails every
        // request — confirms the poll loop's fail-safe leaves pre-seeded
        // allow-lists untouched rather than clearing them.
        let tx_allow_list = Arc::new(std::sync::RwLock::new(HashSet::from(["seed".to_string()])));
        let tx_arm_allow_list =
            Arc::new(std::sync::RwLock::new(HashSet::from(["seed".to_string()])));
        let client_keys = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let arm = Arc::new(Mutex::new(ArmState::new()));
        let audit = AuditLog::new(audit_tmp());
        let revocation_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cqdx_cfg = pancetta_config::network::CqdxConfig {
            enabled: true,
            base_url: "http://127.0.0.1:1".to_string(), // nothing listening — every request fails
            token: Some("pat_test_token_0000000000".to_string()),
            poll_interval_secs: 30,
            authorizations_poll_interval_secs: 45,
        };
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(poll_authorizations_loop(
            cqdx_cfg,
            "agent_this".to_string(),
            std::env::temp_dir(),
            tx_allow_list.clone(),
            tx_arm_allow_list.clone(),
            client_keys.clone(),
            arm,
            audit,
            revocation_pending,
            shutdown_clone,
            Duration::from_millis(50),
        ));
        tokio::time::sleep(Duration::from_millis(150)).await;
        shutdown.store(true, Ordering::Release);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert_eq!(
            *tx_allow_list.read().unwrap(),
            HashSet::from(["seed".to_string()]),
            "a failing poll must never clear the pre-seeded admission allow-list"
        );
        assert_eq!(
            *tx_arm_allow_list.read().unwrap(),
            HashSet::from(["seed".to_string()]),
            "a failing poll must never clear the pre-seeded TX-arm-eligible allow-list"
        );
    }

    // --- station_agent_active: cqdx auto-populate OR-branch (Q-0043 review fix) ---

    fn relay_configured_station_agent_cfg() -> pancetta_config::network::StationAgentConfig {
        pancetta_config::network::StationAgentConfig {
            enabled: true,
            relay_url: Some("wss://relay.example.com".to_string()),
            pairing_api_url: Some("https://relay.example.com/api/v1".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn station_agent_active_true_when_cqdx_will_populate_even_with_empty_static_list() {
        let mut cfg = relay_configured_station_agent_cfg();
        cfg.tx_allow_list = vec![];
        let cqdx_cfg = pancetta_config::network::CqdxConfig {
            enabled: true,
            token: Some("pat_test_token_0000000000".to_string()),
            ..Default::default()
        };
        assert!(station_agent_active(&cfg, &cqdx_cfg));
    }

    #[test]
    fn station_agent_active_false_when_cqdx_disabled_and_static_list_empty() {
        let mut cfg = relay_configured_station_agent_cfg();
        cfg.tx_allow_list = vec![];
        let cqdx_cfg = pancetta_config::network::CqdxConfig::default(); // disabled by default
        assert!(!station_agent_active(&cfg, &cqdx_cfg));
    }

    #[test]
    fn station_agent_active_true_when_static_list_non_empty_cqdx_disabled() {
        let mut cfg = relay_configured_station_agent_cfg();
        cfg.tx_allow_list = vec!["some_client_key".to_string()];
        let cqdx_cfg = pancetta_config::network::CqdxConfig::default();
        assert!(station_agent_active(&cfg, &cqdx_cfg));
    }

    // --- seed_tx_arm_allow_list: CQD-11 / Q-0052 follow-up (fail-closed seed) ---

    /// PR #259 shipped the split with a fail-open seed: with cqdx
    /// auto-populate enabled (so `poll_authorizations_loop` WILL be
    /// spawned), a client present in the static `tx_allow_list` must NOT be
    /// present in the seeded `tx_arm_allow_list` — i.e. must NOT be able to
    /// arm TX — until a poll actually succeeds. Seeding it from the static
    /// list (cloning, PR #259's approach) is fail-OPEN: it trusts every
    /// statically listed client by default instead of trusting none until
    /// cqdx proves who's non-delegated.
    #[test]
    fn cqdx_enabled_seeds_tx_arm_allow_list_empty_regardless_of_static_list() {
        let static_list = HashSet::from(["some_client_key".to_string()]);
        let seeded = seed_tx_arm_allow_list(&static_list, /* cqdx_will_poll */ true);
        assert!(
            seeded.is_empty(),
            "cqdx-enabled must seed tx_arm_allow_list EMPTY (fail-closed) even when the \
             static tx_allow_list is non-empty — nobody may arm TX until a poll succeeds"
        );
    }

    /// Companion: `tx_allow_list` (connection admission) itself is NOT
    /// affected by this fail-closed seed — it stays seeded from the static
    /// config list (unchanged, out of scope for this fix), so a delegated
    /// guest can still connect from the first second, just never arm. When
    /// cqdx is disabled entirely, `tx_arm_allow_list` must still equal the
    /// static list — no delegation concept exists in the pure-config path.
    #[test]
    fn cqdx_disabled_seeds_tx_arm_allow_list_from_static_list() {
        let static_list = HashSet::from(["some_client_key".to_string()]);
        let seeded = seed_tx_arm_allow_list(&static_list, /* cqdx_will_poll */ false);
        assert_eq!(
            seeded, static_list,
            "cqdx-disabled must seed tx_arm_allow_list identically to the static list — \
             no delegation concept exists in the pure-config path"
        );
    }

    // --- Disarm-on-revocation + atomicity: CQD-11 / Q-0052 follow-up (PR #259 gap) ---

    /// `is_tx_arm_eligible` is a live read, never a cached/stale snapshot: a
    /// write landing between two calls IS observed by the second call. This
    /// is the property `verify_and_arm`'s authoritative re-check (taken
    /// while holding `ctx.arm`'s lock, immediately before `st.arm(...)`)
    /// depends on. `verify_and_arm` itself has no `.await`/yield point
    /// between its top-of-function check and this re-check, so an external
    /// test cannot practically force a real race to land inside a single
    /// call — see `concurrent_poll_and_arm_attempts_settle_with_client_disarmed`
    /// below for the genuine end-to-end concurrency test instead.
    #[test]
    fn is_tx_arm_eligible_reflects_the_latest_write_not_a_stale_snapshot() {
        let list = Arc::new(std::sync::RwLock::new(HashSet::from([
            CLIENT_KEY_ID.to_string()
        ])));
        // Simulates verify_and_arm's top-of-function check: passes.
        assert!(is_tx_arm_eligible(&list, CLIENT_KEY_ID));
        // Simulates a concurrent poll cycle revoking eligibility BETWEEN the
        // top-of-function check and the final re-check under arm's lock.
        list.write().unwrap().remove(CLIENT_KEY_ID);
        // The re-check — literally the same function, called again, exactly
        // as verify_and_arm does right before st.arm(...) — must now see the
        // revocation.
        assert!(!is_tx_arm_eligible(&list, CLIENT_KEY_ID));
    }

    /// A client that is ALREADY ARMED and a subsequent authorization poll
    /// comes back WITHOUT that client in the TX-arm-eligible set (here: the
    /// edge is now delegated) must be disarmed immediately — not left to run
    /// out its TTL. `verify_and_arm`'s gate alone is insufficient because it
    /// only runs at arm time; this exercises the REAL
    /// `poll_authorizations_loop` end to end (a genuine mock cqdx HTTP
    /// server, not just a pure-predicate test), since the gap this closes is
    /// specifically about threading the shared `arm` into this task.
    #[tokio::test]
    async fn poll_authorizations_loop_disarms_when_armed_clients_eligibility_is_revoked() {
        // Mock cqdx: every request returns ONE edge for CLIENT_KEY_ID with a
        // non-null delegatedBy — i.e. this refresh says CLIENT_KEY_ID is no
        // longer TX-arm-eligible (still admitted as a peer, though).
        let app = axum::Router::new().route(
            "/api/v1/authorizations",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "authorizations": [{
                        "id": "auth_1",
                        "agentKeyId": "agent_this",
                        "clientKeyId": CLIENT_KEY_ID,
                        "scopes": ["status", "qsy"],
                        "createdAt": "2026-08-15T00:00:00Z",
                        "role": "owner",
                        "delegatedBy": "some_owner_user_id"
                    }]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let tx_allow_list = Arc::new(std::sync::RwLock::new(HashSet::new()));
        let tx_arm_allow_list = Arc::new(std::sync::RwLock::new(HashSet::from([
            CLIENT_KEY_ID.to_string()
        ])));
        let client_keys = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let arm = Arc::new(Mutex::new(ArmState::new()));
        let audit = AuditLog::new(audit_tmp());
        let audit_path = audit.path().clone();
        let revocation_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Pre-arm as CLIENT_KEY_ID — simulates a client that armed BEFORE
        // its edge became delegated.
        {
            let mut st = arm.lock().unwrap();
            st.set_local_consent(true, NOW);
            st.arm(
                pancetta_agent::arm::VerifiedArmGrant {
                    operator_callsign: "K5ARH".to_string(),
                    ttl_ms: 3_600_000, // 1h — long enough that only the poll-driven disarm can end it
                    scope_tx: true,
                    jti: "revocation-test-jti".to_string(),
                    client_key_id: CLIENT_KEY_ID.to_string(),
                },
                NOW,
            );
        }
        assert!(
            arm.lock().unwrap().is_armed(),
            "setup sanity: must be armed before the poll runs"
        );

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cqdx_cfg = pancetta_config::network::CqdxConfig {
            enabled: true,
            base_url: format!("http://{addr}"),
            token: Some("pat_test_token_0000000000".to_string()),
            poll_interval_secs: 30,
            authorizations_poll_interval_secs: 45,
        };
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(poll_authorizations_loop(
            cqdx_cfg,
            "agent_this".to_string(),
            std::env::temp_dir(),
            tx_allow_list.clone(),
            tx_arm_allow_list.clone(),
            client_keys.clone(),
            arm.clone(),
            audit,
            revocation_pending.clone(),
            shutdown_clone,
            Duration::from_millis(50),
        ));
        // Give the poll loop time to complete at least one successful cycle
        // against the mock server.
        tokio::time::sleep(Duration::from_millis(300)).await;
        shutdown.store(true, Ordering::Release);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        assert!(
            tx_allow_list.read().unwrap().contains(CLIENT_KEY_ID),
            "the now-delegated client must still be admitted as a peer"
        );
        assert!(
            !tx_arm_allow_list.read().unwrap().contains(CLIENT_KEY_ID),
            "the now-delegated client must no longer be TX-arm-eligible"
        );
        assert!(
            !arm.lock().unwrap().is_armed(),
            "a live arm whose controller's TX-arm eligibility was just revoked \
             by a poll must be disarmed immediately, not left to run out its TTL"
        );
        assert!(
            revocation_pending.load(Ordering::Acquire),
            "a poll-driven disarm must flag revocation_pending so the session loop \
             broadcasts the updated control state on its next tick (round-1 review fix)"
        );

        let log = std::fs::read_to_string(&audit_path).unwrap_or_default();
        let disarmed = log
            .lines()
            .filter_map(|l| serde_json::from_str::<AuditEvent>(l).ok())
            .any(|ev| {
                ev.kind == AuditKind::Disarmed && ev.detail.contains("authorization-revoked")
            });
        assert!(
            disarmed,
            "the revocation-driven disarm must be audited with the AuthorizationRevoked reason"
        );
    }

    /// Round-1 review fix (PR #261, P2 finding): the disarm-on-revocation
    /// path is not just an `ArmState`/audit-log mutation — it must reach
    /// already-connected peers the same way every OTHER arm transition in
    /// this module does, via a broadcast `controlState { transmitArmed:
    /// false }` frame (rule 7). This drives a REAL session
    /// (`run_one_session`, a real Noise handshake, a real established peer)
    /// and decrypts the actual wire bytes delivered to that peer — not just
    /// `ArmContext`/`ArmState` state or the audit log — proving the poll's
    /// `revocation_pending` flag genuinely reaches the transport layer.
    ///
    /// `run_one_session`'s MockWs-driven tick loop is entirely synchronous
    /// (no task-scheduling yield point outside `dispatch_action`'s own
    /// internal awaits), so a real concurrent poll task cannot be reliably
    /// interleaved at a precise tick boundary in a test. `TriggerAfterWs`
    /// deterministically places "the poll fired here" immediately before the
    /// THIRD scripted frame is served — i.e. AFTER the peer has already
    /// established (so its stale `transmitArmed: true` greeting is on the
    /// wire) but BEFORE the next tick, exactly modeling "a poll disarmed the
    /// live controller between two ticks."
    #[tokio::test]
    async fn poll_driven_disarm_broadcasts_updated_control_state_to_connected_peers() {
        let identity = AgentIdentity::generate();
        let agent_kid = identity.key_id();
        let agent_pub = identity.agreement_public_raw();

        let mut allow = HashSet::new();
        allow.insert(CLIENT_KEY_ID.to_string());
        let mut ctx = fresh_ctx(&agent_kid, allow);

        // Pre-arm as CLIENT_KEY_ID (consent + a live grant) BEFORE the
        // session starts — so the peer's establishment greeting shows
        // `transmitArmed: true`, giving the later broadcast a real
        // true→false transition to prove. `run_one_session`'s internal
        // `control_state_sends` calls always use the REAL wall clock
        // (`now_ms()`), not this module's fixed `NOW` test constant, so the
        // grant must be armed against real "now" too or it reads as already
        // TTL-expired.
        let real_now = now_ms();
        ctx.arm.lock().unwrap().set_local_consent(true, real_now);
        ctx.arm.lock().unwrap().arm(
            pancetta_agent::arm::VerifiedArmGrant {
                operator_callsign: OPERATOR.to_string(),
                ttl_ms: 3_600_000,
                scope_tx: true,
                jti: "broadcast-test-jti".to_string(),
                client_key_id: CLIENT_KEY_ID.to_string(),
            },
            real_now,
        );
        assert!(
            ctx.arm.lock().unwrap().tx_permitted(real_now),
            "setup sanity: must be armed+permitted before the peer establishes"
        );

        // Clones of the shared state a concurrent poll task would hold —
        // moved into the trigger closure below to simulate its disarm.
        let poll_arm = ctx.arm.clone();
        let poll_revocation_pending = ctx.revocation_pending.clone();

        let bus = MessageBus::new(64).unwrap();
        let mut initiator = fresh_initiator(&agent_pub);
        let msg1 = initiator.write_msg1(b"");

        let hello = RelayFrame::Hello {
            challenge: b64url(&[7u8; 32]),
        }
        .to_json()
        .unwrap();
        let env_msg1 = RelayFrame::Env {
            dst: agent_kid.clone(),
            payload: b64url(&msg1),
            src: Some(CLIENT_KEY_ID.to_string()),
        }
        .to_json()
        .unwrap();
        // A benign frame that produces a `Poll::Idle` tick without touching
        // `ctx.peers` — the tick on which the trigger fires and this
        // module's new post-tick `revocation_pending` check runs.
        let idle_tick = RelayFrame::Presence {
            peer: "irrelevant-peer".to_string(),
            state: "up".to_string(),
        }
        .to_json()
        .unwrap();

        let outbound = Arc::new(Mutex::new(Vec::new()));
        let mock = MockWs::new(vec![hello, env_msg1, idle_tick], outbound.clone());
        // Fires right before the 3rd scripted frame (idle_tick) is served —
        // i.e. AFTER CLIENT_KEY_ID has already established.
        let ws = TriggerAfterWs::new(mock, 3, move || {
            let effects = poll_arm
                .lock()
                .unwrap()
                .disarm_with_reason(DisarmReason::AuthorizationRevoked, now_ms());
            assert!(
                !effects.is_empty(),
                "setup sanity: the simulated poll disarm must actually find a live arm"
            );
            poll_revocation_pending.store(true, Ordering::Release);
        });

        run_one_session(ws, &identity, &mut ctx, &bus, &mut None).await;

        assert!(
            ctx.peers.contains_key(CLIENT_KEY_ID),
            "the peer must have established before the simulated poll disarm"
        );
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "setup sanity: the simulated poll disarm must have taken effect"
        );

        // Every env addressed to CLIENT_KEY_ID, in wire order: msg2, the
        // establishment greeting (armed=true), then the poll-driven
        // broadcast (armed=false).
        let out = outbound.lock().unwrap().clone();
        let envs: Vec<Vec<u8>> = out
            .iter()
            .filter_map(|s| match parse_frame(s).unwrap() {
                RelayFrame::Env { dst, payload, .. } if dst == CLIENT_KEY_ID => Some(
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(&payload)
                        .unwrap(),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(
            envs.len(),
            3,
            "msg2 + the establishment greeting + the poll-driven broadcast"
        );

        initiator.read_msg2(&envs[0]);
        let mut transport = initiator.into_transport();
        let mut decrypt = |ct: &[u8]| -> serde_json::Value {
            let mut buf = vec![0u8; ct.len().max(1)];
            let n = transport.read_message(ct, &mut buf).unwrap();
            buf.truncate(n);
            serde_json::from_slice(&buf).unwrap()
        };

        let greeting = decrypt(&envs[1]);
        assert_eq!(greeting["event"]["event"], "controlState");
        assert_eq!(
            greeting["event"]["transmitArmed"], true,
            "the establishment greeting must reflect the pre-armed state"
        );

        let broadcast = decrypt(&envs[2]);
        assert_eq!(broadcast["event"]["event"], "controlState");
        assert_eq!(
            broadcast["event"]["transmitArmed"], false,
            "the poll-driven disarm must broadcast an updated controlState with \
             transmitArmed: false to the already-connected peer — not just mutate \
             ArmState/the audit log"
        );
    }

    /// End-to-end concurrent stress test on a multi-threaded runtime. Runs
    /// the REAL `poll_authorizations_loop` — against a mock cqdx server that
    /// reports CLIENT_KEY_ID's edge as delegated (revoked) on EVERY response
    /// — concurrently with a task repeatedly attempting to arm as
    /// CLIENT_KEY_ID through the REAL `dispatch_action`/`verify_and_arm`
    /// path. These are the exact same two tasks (session loop vs. poll
    /// task) that race in production, sharing the exact same `Arc<Mutex<
    /// ArmState>>` / `Arc<RwLock<HashSet<String>>>` handles. Once both
    /// settle, the client must not be left armed.
    ///
    /// Honesty check on what this DOES and DOES NOT prove: the atomicity
    /// race this fix closes is a genuine *thread*-level race over a handful
    /// of CPU instructions between two lock acquisitions inside a single
    /// poll iteration (there is no `.await` — no task-scheduling yield point
    /// — between `poll_authorizations_loop`'s disarm-check and its publish,
    /// nor between `verify_and_arm`'s top-of-function check and its
    /// `arm.lock()` acquisition). This test demonstrates real end-to-end
    /// soundness under concurrent load (a genuine regression test for the
    /// disarm-on-revocation + arm pipeline working together) but does NOT
    /// reliably land inside that microsecond-scale window —
    /// `is_tx_arm_eligible_reflects_the_latest_write_not_a_stale_snapshot`
    /// above is the test that actually pins the mechanism (live re-read, not
    /// a stale snapshot) the fix depends on. This test IS, however, the one
    /// that caught a genuine self-deadlock while this fix was being written
    /// (an un-dropped `tx_arm_allow_list` read guard in `verify_and_arm`
    /// before its re-check tried to acquire the same `RwLock` again — see
    /// the `drop(allow_list)` comment there) — real concurrency exercise
    /// remains valuable even when it can't pin a specific micro-race.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_poll_and_arm_attempts_settle_with_client_disarmed() {
        let app = axum::Router::new().route(
            "/api/v1/authorizations",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "authorizations": [{
                        "id": "auth_1",
                        "agentKeyId": AGENT_KEY_ID,
                        "clientKeyId": CLIENT_KEY_ID,
                        "scopes": ["status", "qsy", "tx"],
                        "createdAt": "2026-08-15T00:00:00Z",
                        "role": "owner",
                        "delegatedBy": "some_owner_user_id"
                    }]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let poll_arm = ctx.arm.clone();
        let poll_tx_arm_allow_list = ctx.tx_arm_allow_list.clone();
        let poll_tx_allow_list = ctx.tx_allow_list.clone();
        let poll_client_keys = ctx.client_keys.clone();
        let poll_audit = ctx.audit.clone();
        let poll_revocation_pending = ctx.revocation_pending.clone();

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cqdx_cfg = pancetta_config::network::CqdxConfig {
            enabled: true,
            base_url: format!("http://{addr}"),
            token: Some("pat_test_token_0000000000".to_string()),
            poll_interval_secs: 30,
            authorizations_poll_interval_secs: 45,
        };
        let shutdown_clone = shutdown.clone();
        // Fast poll interval so MANY cycles run during the arm-attempt loop
        // below, maximizing the chance of actually landing inside the race
        // window on any given test run — the final assertion holds
        // regardless, but this makes the test exercise the interesting
        // interleavings, not just the trivially-safe ones.
        let poll_handle = tokio::spawn(poll_authorizations_loop(
            cqdx_cfg,
            AGENT_KEY_ID.to_string(),
            std::env::temp_dir(),
            poll_tx_allow_list,
            poll_tx_arm_allow_list,
            poll_client_keys,
            poll_arm,
            poll_audit,
            poll_revocation_pending,
            shutdown_clone,
            Duration::from_millis(5),
        ));

        let bus = MessageBus::new(64).unwrap();
        // Repeatedly attempt to arm as CLIENT_KEY_ID, racing the poll above.
        // A fresh jti each time (single-use jti gate is unrelated to this
        // race). `yield_now` gives the poll task's OS thread real chances to
        // interleave on the multi-threaded runtime.
        for i in 0..100 {
            dispatch_action(
                arm_action(&format!("race-jti-{i}")),
                CLIENT_KEY_ID,
                &mut ctx,
                &bus,
                SESSION_ID,
                NOW,
            )
            .await;
            tokio::task::yield_now().await;
        }

        // Give the poll loop several more cycles to run AFTER the last arm
        // attempt, so its disarm-check has a final chance to react to any
        // residual armed state before we assert.
        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.store(true, Ordering::Release);
        let _ = tokio::time::timeout(Duration::from_secs(2), poll_handle).await;

        assert!(
            !ctx.tx_arm_allow_list
                .read()
                .unwrap()
                .contains(CLIENT_KEY_ID),
            "setup sanity: the poll must have published CLIENT_KEY_ID's revocation by now"
        );
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "regardless of how the arm-attempt loop interleaved with the poll's \
             revocation, the client must never be left armed once both settle"
        );
    }
}
