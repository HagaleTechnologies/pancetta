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
//!   terminal code is the first line of defense). The peer must then also
//!   prove, inside the Noise handshake, that it holds the keys for that keyId
//!   (PAN-95: a msg1 client identity proof bound to its Noise static key);
//!   the relay's `src` label alone never establishes a peer. Admission stays
//!   live after that (PAN-96): a peer an authorization refresh drops is
//!   refused at dispatch and sent nothing from the moment the new list is
//!   published, and is evicted from the demux (transport dropped, hello token
//!   deny-listed) on the session's next tick; a newly listed peer may connect
//!   without a reconnect.
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
//!   unchanged. Since PAN-95 an established peer's INBOUND relay stream is
//!   EMPTY until it sends a valid `hello`; from then on it receives a
//!   `ControlState` greeting plus any read-stream events, both carried by
//!   pre-existing rig-api.v1 types (no new wire event). A control-mutating action from a non-controller while
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
//!   coordinator's shared `remote_tx_arm` [`ArmState`]. The arm is refused
//!   unless THIS channel's own local consent (`remote_tx_enabled`) is ON —
//!   consent is tracked per channel, so the WSJT-X UDP bridge's
//!   `allow_tx_initiation` never satisfies it. Once armed, a station-agent
//!   `TxOrigin::Remote` transmit request will key PTT at the TX worker's arm
//!   gate, which re-checks this channel's consent at key-time.
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
//!   sends each event as `ServerFrame::Event` JSON inside per-peer encrypted
//!   `env` frames to every established peer *holding a currently-valid
//!   `status`-or-higher capability from a verified `hello`* (PAN-95,
//!   [`peer_may_receive`]); a peer without one is sent no frame of any kind. The feed is started
//!   when *either* the localhost gateway or the station agent is enabled
//!   (`display_feed_enabled`); the tokio `broadcast` ring is a drop-oldest
//!   queue (lossy by design — control frames are never queued behind it).
//!   Per-peer `ControlState` (`controlHeldByMe`, `transmitArmed`) is sent on
//!   every controller/arm transition and as a greeting on a successful
//!   `hello` — no new wire event was needed; rig-api.v1 already defined it.
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
//! Per-channel local consent is still seeded into the arm from config at
//! startup (so the gate reflects `remote_tx_enabled` even when the transport
//! is off), matching the coordinator's constructor seeding — this is
//! idempotent.

pub mod net;

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ed25519_dalek::VerifyingKey;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use pancetta_agent::arm::{ArmEffect, ArmState, ArmTime, ConsentChannel, DisarmReason};
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
    /// **Best-effort** revocation deny-list, keyed by the capabilityToken's
    /// `jti` (frozen e2e-auth.v1 §6-revocation). The station-local
    /// TX-allow-list stays the authoritative revoke; this set only ever
    /// narrows it. Populated (PAN-96) by [`reconcile_authorizations`] with the
    /// hello-token `jti` of every peer an authorization refresh evicts, so the
    /// same token is refused at `hello` ([`dispatch_hello`]), at use
    /// ([`PeerCtx::grants`]) and at arm (`verify_arm_grant` → `CapError::Revoked`)
    /// until the client fetches a fresh one. Process-lifetime: a StationAgent
    /// restart clears it (the token's own `exp` still bounds it). SEAM: a
    /// future cqdx-fed deny-list would also populate this set; it fails OPEN
    /// (stays as-is) when the station is offline, per the contract's
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
    /// allocated AND the peer proved, inside the Noise handshake, that it holds
    /// the keys for that keyId (PAN-95), so a key in this map always names a
    /// vetted, allow-listed identity proven in the handshake, never a bare
    /// relay claim. Cleared at the start of every new
    /// session (`run_one_session`) — no established peer or granted scope
    /// carries across a reconnect, so a different set of allow-listed clients
    /// can connect next time.
    ///
    /// An entry does NOT imply the peer is still admitted: the admission list
    /// can change under a live session (PAN-96). [`peer_admitted`] is the live
    /// check every scoped action and outbound frame goes through, and
    /// [`reconcile_authorizations`] removes the entry of a peer an
    /// authorization refresh dropped.
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
    /// The authorization generation (PAN-96): bumped by
    /// `poll_authorizations_loop`, after it publishes them, whenever a poll
    /// changes `tx_allow_list` or `tx_arm_allow_list`. Shared with the poll
    /// task (a SEPARATE tokio task, like `revocation_pending`); never bumped in
    /// static-config mode.
    authz_generation: Arc<std::sync::atomic::AtomicU64>,
    /// The generation THIS session last reconciled to. When it lags
    /// `authz_generation`, [`reconcile_authorizations`] applies the refresh to
    /// the live session on the next tick.
    authz_generation_seen: u64,
    /// The arm-timing clock (wall + monotonic). Production uses
    /// [`ArmTime::now`]; session tests inject a manually advanced timeline.
    clock: ArmClock,
}

/// Source of [`ArmTime`] samples for the session loop (see
/// `ArmContext::clock`).
type ArmClock = Arc<dyn Fn() -> ArmTime + Send + Sync>;

/// A verified hello capability, kept whole enough to re-check at use time:
/// every use re-checks admission ([`peer_admitted`]), expiry, the revocation
/// deny-list and scope (PAN-96).
struct PeerAuth {
    /// Scopes granted by the token (rig-api tiers `status` ⊂ `qsy` ⊂ `tx`).
    scopes: Vec<String>,
    /// The token's expiry (ms). The capability is usable only while `now < exp_ms`.
    exp_ms: i64,
    /// The token's `jti` — its identity, for the station's revocation deny-list.
    jti: String,
}

/// Per-established-peer session state (Task 4: one entry per demuxed peer,
/// replacing the single-peer-era `ArmContext::hello_scopes` scalar).
struct PeerCtx {
    /// The capability from THIS peer's most recent verified
    /// `hello.capabilityToken`. `None` ⇒ the peer has proven its identity
    /// (PAN-95, `MultiPeerSession`) but holds no currently-usable capability:
    /// it is sent NOTHING and may run no scoped action — the state on
    /// establishment, and the fail-closed state after any invalid/mismatched
    /// token or an expiry sweep. A `hello`'s `capabilityToken.clientKeyId`
    /// MUST equal the ACTUAL sending peer (the identity `MultiPeerSession`
    /// proved in the handshake, never a session-global value) before it is
    /// honored (Q-0019 #5).
    auth: Option<PeerAuth>,
}

impl PeerCtx {
    /// A freshly established peer: identity proven, nothing authorized yet.
    fn unauthorized() -> Self {
        Self { auth: None }
    }

    /// Whether this peer's capability currently holds `required` (rig-api
    /// tiers) — it must be unexpired at `now` and its `jti` must not be on the
    /// station's revocation deny-list. Admission is checked separately, live,
    /// by [`peer_grants`].
    fn grants(&self, required: &str, now: i64, revoked: &HashSet<String>) -> bool {
        self.auth.as_ref().is_some_and(|a| {
            now < a.exp_ms && !revoked.contains(&a.jti) && scope_at_least(&a.scopes, required)
        })
    }
}

/// Whether `peer` is CURRENTLY on the station's connection-admission list — a
/// live read, fail closed on a poisoned lock (PAN-96).
fn peer_admitted(ctx: &ArmContext, peer: &str) -> bool {
    ctx.tx_allow_list
        .read()
        .map(|l| l.contains(peer))
        .unwrap_or(false)
}

/// Whether established peer `peer` currently holds `required` at `now`: it is
/// still admitted, and its capability is unexpired, not deny-listed and
/// scoped for `required`.
fn peer_grants(ctx: &ArmContext, peer: &str, required: &str, now: i64) -> bool {
    peer_admitted(ctx, peer)
        && ctx
            .peers
            .get(peer)
            .is_some_and(|p| p.grants(required, now, &ctx.revoked_jtis))
}

/// THE outbound gate (PAN-95): a peer is sent a frame — any frame — only while
/// it is still admitted and holds a currently-valid `status`-or-higher
/// capability from a verified hello (PAN-96: re-checked per frame, so a peer
/// an authorization refresh dropped is sent nothing even before its eviction
/// runs). Both `deliver_sends` and `drain_read_stream` consult this; they are
/// the only two station→client paths.
fn peer_may_receive(ctx: &ArmContext, peer: &str, now: i64) -> bool {
    peer_grants(ctx, peer, "status", now)
}

/// Clear the capability of every peer whose token has expired at `now`, and
/// return those peers (each exactly once — a cleared peer is not returned
/// again). The predicates above check `exp` on their own, so this is for
/// diagnostics and state freshness, not safety.
fn sweep_expired_peer_auth(ctx: &mut ArmContext, now: i64) -> Vec<String> {
    let mut expired = Vec::new();
    for (peer, p) in ctx.peers.iter_mut() {
        if p.auth.as_ref().is_some_and(|a| a.exp_ms <= now) {
            p.auth = None;
            expired.push(peer.clone());
        }
    }
    expired
}

/// Run [`sweep_expired_peer_auth`], log each newly-expired peer once, and —
/// if the expired peer is the current controller — disarm and release control
/// (fail TX-off, as on controller loss): a controller the station can no
/// longer send anything to must not keep transmitting blind. Returns the
/// updated `controlState` frames for the caller to deliver (empty when no
/// controller expired).
fn expire_peer_auth(ctx: &mut ArmContext, now: ArmTime) -> Vec<PeerSend> {
    let mut sends = Vec::new();
    for peer in sweep_expired_peer_auth(ctx, now.unix_ms) {
        info!(target: "agent.control", peer = %peer,
            "peer capability expired — station data paused until it sends a hello with a fresh capabilityToken");
        if ctx.controller.as_deref() == Some(peer.as_str()) {
            let effects = match ctx.arm.lock() {
                Ok(mut st) => st.disarm(now.unix_ms),
                Err(_) => Vec::new(),
            };
            if !effects.is_empty() {
                warn!(target: "agent.tx", peer = %peer, "controller capability expired with a live arm — disarming");
            }
            apply_arm_effects(&ctx.audit, &effects);
            ctx.controller = None;
            sends = control_state_sends(ctx, now, None);
        }
    }
    sends
}

/// PAN-96: apply a live authorization refresh to THIS session. A no-op until
/// `poll_authorizations_loop` bumps `authz_generation` (it does so only when an
/// admission list actually changed, and only after publishing it). Then, in one
/// synchronous step on the task that owns the session:
/// - the demux's admission set is replaced with the live `tx_allow_list` (a
///   poisoned lock reads as empty: fail closed, everyone is evicted), so a
///   newly authorized client can establish without a reconnect;
/// - every peer the live list no longer holds is evicted: its Noise transport
///   is dropped (re-admission needs a fresh handshake), its `PeerCtx` is
///   removed, and its hello token's `jti` is deny-listed (it must fetch a
///   fresh token);
/// - if an evicted peer held control or the live arm, the arm is disarmed
///   (`authorization-revoked`; a no-op when the poll task already did), a
///   revoked controller is released, and the updated `controlState` frames
///   for the remaining peers are returned for the caller to deliver.
///
/// There is no `.await` here, so no frame from an evicted peer can be
/// processed between its demux eviction and its `PeerCtx` removal.
fn reconcile_authorizations<W: pancetta_agent::relay::WsConn>(
    ctx: &mut ArmContext,
    sess: &mut MultiPeerSession<'_, W>,
    now: ArmTime,
) -> Vec<PeerSend> {
    let current = ctx.authz_generation.load(Ordering::Acquire);
    if current == ctx.authz_generation_seen {
        return Vec::new();
    }
    let allow = ctx
        .tx_allow_list
        .read()
        .map(|l| l.clone())
        .unwrap_or_default();
    let mut evicted = sess.replace_allowed(allow.clone());
    let mut stale: Vec<String> = ctx
        .peers
        .keys()
        .filter(|p| !allow.contains(*p) && !evicted.contains(*p))
        .cloned()
        .collect();
    stale.sort();
    evicted.extend(stale);
    for peer in &evicted {
        if let Some(auth) = ctx.peers.remove(peer).and_then(|pc| pc.auth) {
            ctx.revoked_jtis.insert(auth.jti);
        }
        warn!(target: "agent.control", peer = %peer,
            "remote peer evicted — its authorization was revoked; it must be re-authorized and reconnect");
    }

    let controller_evicted = ctx.controller.as_ref().is_some_and(|c| evicted.contains(c));
    let armer_evicted = {
        let mut st = match ctx.arm.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let armer_evicted = st
            .armed_client_key_id()
            .is_some_and(|k| evicted.iter().any(|p| p == k));
        if controller_evicted || armer_evicted {
            let effects = st.disarm_with_reason(DisarmReason::AuthorizationRevoked, now.unix_ms);
            if !effects.is_empty() {
                warn!(target: "agent.tx", "controller evicted with a live arm — disarming");
            }
            apply_arm_effects(&ctx.audit, &effects);
        }
        armer_evicted
    };
    let mut sends = Vec::new();
    if controller_evicted || armer_evicted {
        if controller_evicted {
            ctx.controller = None;
        }
        sends = control_state_sends(ctx, now, None);
    }
    ctx.authz_generation_seen = current;
    sends
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

/// Unix milliseconds now, for audit timestamps on arm-timing-free paths
/// (explicit disarms). Deadline checks use an [`ArmTime`] sample instead.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Turn the effects of an arm transition into durable audits and, when the
/// arm actually disarmed, the existing `controlState` fan-out so every
/// established peer learns `transmitArmed: false` straight away. Shared by the
/// per-iteration expiry sweep and the heartbeat path. Call with the arm lock
/// released.
fn arm_transition_sends(ctx: &ArmContext, effects: &[ArmEffect], now: ArmTime) -> Vec<PeerSend> {
    apply_arm_effects(&ctx.audit, effects);
    if effects
        .iter()
        .any(|e| matches!(e, ArmEffect::Disarmed { .. }))
    {
        control_state_sends(ctx, now, None)
    } else {
        Vec::new()
    }
}

/// A fresh [`ArmTime`], never earlier than `floor` (an earlier sample) on
/// either clock. Arm mutations take it with the arm lock already held so
/// verification or a lock wait cannot make it stale; the session loop takes
/// it after a delivery that may have blocked.
fn fresh_arm_time(ctx: &ArmContext, floor: ArmTime) -> ArmTime {
    let fresh = (ctx.clock)();
    ArmTime {
        unix_ms: fresh.unix_ms.max(floor.unix_ms),
        monotonic: fresh.monotonic.max(floor.monotonic),
        suspend_aware_ms: fresh.suspend_aware_ms.max(floor.suspend_aware_ms),
    }
}

/// Dead-man / TTL sweep for the session loop (PAN-97): expire a lapsed arm
/// even when no heartbeat or TX request arrives. Dead-man expiry removes arm
/// authority only — the controller slot, the peers and `seen_jtis` stay, so
/// recovery is a fresh signed arm on the same session. A poisoned lock is left
/// to the read-only gate, which already denies. The deadline check uses a
/// fresh sample taken under the arm lock (floored at `now`).
fn sweep_arm_expiry(ctx: &ArmContext, now: ArmTime) -> Vec<PeerSend> {
    let effects = match ctx.arm.lock() {
        Ok(mut st) => st.tick(fresh_arm_time(ctx, now)),
        Err(_) => Vec::new(),
    };
    if !effects.is_empty() {
        debug!(target: "agent.tx", "remote arm deadline passed — auto-disarmed");
    }
    arm_transition_sends(ctx, &effects, now)
}

/// Last-resort fail-safe for the live station-agent task. The guard is bound
/// before session state is constructed and therefore disarms during unwinding
/// from any panic in the session loop. Drop must remain synchronous and
/// panic-free: it can itself run while another panic is already unwinding.
///
/// `normal_exit` lets `Drop` tell an ordinary, requested shutdown apart from
/// an actual crash/unwind (PAN-19 MEDIUM #4): `run_session_loop` already
/// calls `disarm_on_loss` (reason `OperatorDisarm`) on every ordinary exit
/// path, including its own "final safety" call right before it returns -- so
/// on a clean shutdown this guard's `Drop` is normally a no-op (the arm is
/// already disarmed, and `disarm_with_reason` no-ops when nothing is armed).
/// But if that prior disarm silently gave up (e.g. a poisoned `arm` mutex),
/// the session could still be armed when this `Drop` runs even though
/// nothing crashed -- attributing that disarm to `ComponentCrash`
/// unconditionally would misattribute a routine shutdown as a crash in the
/// audit log, exactly the failure mode `DisarmReason` exists to prevent (see
/// its doc comment), just in the opposite direction.
///
/// `normal_exit` is set by [`RemoteTxDisarmGuard::mark_normal_exit`], called
/// only from `run_session_loop`'s own clean-return path, immediately before
/// it returns. PAN-36: an earlier version of this guard instead read the
/// process-wide `shutdown` `AtomicBool` here in `Drop` -- but that flag can
/// be set true by an unrelated task at any moment, including the exact
/// instant this task is unwinding from a genuine panic, which misattributed
/// the crash as `OperatorDisarm`. `normal_exit` is owned exclusively by this
/// task and flipped only on its own verified clean-return path, so it can't
/// be raced by anything outside this task.
///
/// PAN-36 round 2: `normal_exit` alone under-covers a second ordinary exit
/// path -- `run_one_session` never checks `cfg.shutdown` itself, so a
/// session with an established peer can still be mid-flight when shutdown is
/// requested. `shutdown.rs` gives every named task a 1s grace period, then
/// force-`abort()`s it. That abort drops this guard immediately, well before
/// `mark_normal_exit`, via ordinary (non-unwinding) `Drop` cleanup -- so a
/// genuine panic always leaves this thread `std::thread::panicking()`, while
/// an abort-triggered cancellation never does.
///
/// PAN-36 round 3 (Codex on the round-2 fix): treating every non-panicking
/// drop as automatically operator-initiated is itself too broad -- some
/// future cancellation unrelated to shutdown (a different `AbortHandle`
/// call, a runtime-level drop) would also read as "not panicking" and get
/// misattributed as `OperatorDisarm`, hiding a real unexpected termination.
/// `Drop` now ALSO re-checks the shared `shutdown` flag for that fallback
/// case, so a non-panicking, non-`normal_exit` drop is `OperatorDisarm` only
/// when shutdown was actually requested. This does not reopen the round-1
/// race (an unrelated task's shutdown flag flipping during a real panic on
/// THIS task): `!std::thread::panicking()` is checked FIRST and short-
/// circuits to `ComponentCrash` on any genuine panic regardless of the
/// flag's state, exactly as round 2 established -- the flag is now only
/// consulted to narrow the *non-panicking* fallback, never to override a
/// real panic.
struct RemoteTxDisarmGuard {
    arm: Arc<Mutex<ArmState>>,
    audit: AuditLog,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    normal_exit: bool,
}

impl RemoteTxDisarmGuard {
    /// Call only right before `run_session_loop` returns on its own
    /// requested-shutdown path -- never from anywhere a panic could still
    /// unwind through afterward.
    fn mark_normal_exit(&mut self) {
        self.normal_exit = true;
    }
}

impl Drop for RemoteTxDisarmGuard {
    fn drop(&mut self) {
        let non_panicking_shutdown_cancellation =
            !std::thread::panicking() && self.shutdown.load(Ordering::Acquire);
        let reason = if self.normal_exit || non_panicking_shutdown_cancellation {
            DisarmReason::OperatorDisarm
        } else {
            DisarmReason::ComponentCrash
        };
        let effects = match self.arm.lock() {
            Ok(mut state) => state.disarm_with_reason(reason, now_ms()),
            Err(poisoned) => poisoned.into_inner().disarm_with_reason(reason, now_ms()),
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
/// [`MultiPeerSession::broadcast_to`] directly (see [`drain_read_stream`]).
/// Every frame, either way, is withheld from a peer [`peer_may_receive`]
/// refuses.
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
/// greeting to a peer whose `hello` just verified. `only = Some(p)` targets
/// just peer `p` (the greeting case); `None` fans out one tailored frame per
/// established peer (each with its own `controlHeldByMe`). Frames are built
/// for every peer; [`deliver_sends`] withholds them from unauthorized ones.
/// `transmitArmed` is evaluated against a fresh sample taken under the arm
/// lock (floored at `now`), so an arm accepted after `now` reads as live.
/// It answers "could THIS channel's TX key right now?", so it reads the
/// station-agent channel's own consent bit.
fn control_state_sends(ctx: &ArmContext, now: ArmTime, only: Option<&str>) -> Vec<PeerSend> {
    let armed = ctx
        .arm
        .lock()
        .map(|s| s.tx_permitted(fresh_arm_time(ctx, now), ConsentChannel::StationAgent))
        .unwrap_or(false);
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
fn controller_gate(ctx: &mut ArmContext, peer: &str, now: ArmTime) -> ControllerGate {
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
                ts_unix_ms: now.unix_ms,
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
/// Pure with respect to timing (takes an [`ArmTime`] sample); side effects are the arm
/// mutation, audit writes, and best-effort bus sends. This is the security spine
/// of the component and is unit-tested directly.
///
/// - `Arm { capability_token, grant }` → verify the capabilityToken as a
///   SEPARATE input (against the pinned IdP key) then verify the client-signed
///   grant (fail-closed, including the `txEnabledUntil` clock-2 gate + the
///   best-effort deny-list), then require this channel's own consent
///   (`remote_tx_enabled`) → `arm.arm()` on success, audited `TxDenied` on any
///   verification error or missing consent (NEVER arms on failure). The token is NOT extracted from
///   the grant — `clientSig` signs only the grant, which references the token
///   via `capabilityJti`.
/// - `Heartbeat` → `arm.heartbeat(arm_jti, seq)`: slides the dead-man window
///   only for a heartbeat that names the current arm (`arm_jti`) with a
///   per-arm-monotonic `seq`; a replayed/wrong-arm heartbeat is rejected (audited
///   `TxDenied`) and the window is NOT slid (contract `$defs.txHeartbeat`). A
///   heartbeat at or after the deadline expires the arm instead (PAN-97) and
///   the resulting `controlState` goes to every peer; only a fresh signed arm
///   restores TX.
/// - `Disarm { arm_jti }` → `arm.disarm()` + audit (fail-safe TX-OFF). The
///   `arm_jti` is a sanity match, NOT a security gate: disarm-any always
///   proceeds; a non-empty arm_jti that doesn't match the live arm still
///   disarms but logs a `warn!`.
/// - `Qsy` / `SetSplit` → coordinator `RigControlMessage` (NON-TX rig control).
/// - `TxRequest(_)` → refused (audited `TxDenied`) while this channel's own
///   consent (`remote_tx_enabled`) is off; otherwise audited `TxRequested` +
///   routed into the REAL QSO engine via a `remote_origin = true`
///   `QsoMessage` (P3.4c). Creating the QSO is
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
    now: ArmTime,
) -> DispatchOutcome {
    // PAN-96: a peer no longer on the live admission list may only give up
    // authority (Disarm / ReleaseControl — fail-safe). Everything else, even a
    // fresh hello or a control grab, is refused before any gate below runs.
    if !matches!(
        action,
        ControlAction::Disarm { .. } | ControlAction::ReleaseControl
    ) && !peer_admitted(ctx, peer)
    {
        warn!(target: "agent.control", peer = %peer,
            "control frame refused — peer is no longer authorized on this station");
        return DispatchOutcome::cont();
    }
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
                        ts_unix_ms: now.unix_ms,
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
            //
            // PAN-97: a heartbeat arriving at or after the deadline expires the
            // arm instead of refreshing it; that transition is broadcast like
            // any other disarm, and only a fresh signed arm restores TX.
            let effects = match ctx.arm.lock() {
                Ok(mut st) => st.heartbeat(&arm_jti, seq, fresh_arm_time(ctx, now)),
                Err(_) => Vec::new(),
            };
            DispatchOutcome::cont_with(arm_transition_sends(ctx, &effects, now))
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
                Ok(mut st) => st.disarm(now.unix_ms),
                Err(_) => Vec::new(),
            };
            apply_arm_effects(&ctx.audit, &effects);
            // Rule 7: every disarm emits controlState (transmit_armed now false).
            DispatchOutcome::cont_with(control_state_sends(ctx, now, None))
        }
        ControlAction::Hello { capability_token } => {
            // Rule 8: Hello scope verification is per-peer — it is NOT a
            // control-mutating action and never grabs. A verified hello is the
            // first moment the peer may receive anything (PAN-95), so the
            // controlState greeting is sent here rather than on establishment.
            // A REFUSED hello that erased the current controller's capability
            // disarms and releases control (PAN-96, [`clear_peer_auth`]) — its
            // updated controlState frames for the remaining peers ride out
            // here too.
            let (verified, mut sends) = dispatch_hello(capability_token, peer, ctx, now.unix_ms);
            if verified {
                sends.extend(control_state_sends(ctx, now, Some(peer)));
            }
            DispatchOutcome::cont_with(sends)
        }
        ControlAction::Qsy { vfo, frequency_hz } => {
            let grab = match controller_gate(ctx, peer, now) {
                ControllerGate::Proceed(sends) => sends,
                ControllerGate::Refused(sends) => return DispatchOutcome::cont_with(sends),
            };
            if !peer_grants(ctx, peer, "qsy", now.unix_ms) {
                warn!(target: "agent.control", peer = %peer, "setFrequency refused — no currently-valid qsy-scoped hello capability for this peer");
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
            if !peer_grants(ctx, peer, "qsy", now.unix_ms) {
                warn!(target: "agent.control", peer = %peer, "setSplit refused — no currently-valid qsy-scoped hello capability for this peer");
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
            //
            // Intent acceptance still requires THIS channel's own consent
            // (`[network.station_agent].remote_tx_enabled`): while it is off
            // the request is refused (audited `TxDenied`) and no QSO is
            // created. Another channel's consent never satisfies this check.
            // A poisoned arm lock refuses too (fail-closed).
            let detail = match &kind {
                TxKind::CallStation { callsign, .. } => format!("callStation {callsign}"),
                TxKind::AnswerCaller { callsign, step, .. } => {
                    format!("answerCaller {callsign} step={step}")
                }
                TxKind::StartCq { offset_hz } => format!("startCq offset={offset_hz}"),
            };
            let (consented, operator_callsign) = match ctx.arm.lock() {
                Ok(st) => (
                    st.channel_consent(ConsentChannel::StationAgent),
                    st.operator_callsign().map(str::to_string),
                ),
                Err(_) => (false, None),
            };
            if !consented {
                ctx.audit.append(&AuditEvent {
                    ts_unix_ms: now.unix_ms,
                    kind: AuditKind::TxDenied,
                    operator_callsign,
                    detail: format!(
                        "txRequest refused: station-agent remote TX consent off ({detail})"
                    ),
                });
                warn!(target: "agent.tx", peer = %peer, request = %detail, "txRequest refused — station-agent remote TX consent is off (remote_tx_enabled=false)");
                return DispatchOutcome::cont_with(grab);
            }
            ctx.audit.append(&AuditEvent {
                ts_unix_ms: now.unix_ms,
                kind: AuditKind::TxRequested,
                operator_callsign: ctx
                    .arm
                    .lock()
                    .ok()
                    .and_then(|s| s.operator_callsign().map(str::to_string)),
                detail: detail.clone(),
            });
            let qso_msg = tx_kind_to_qso_message(kind, peer);
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
                    Ok(mut st) => st.disarm(now.unix_ms),
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
                    Ok(mut st) => st.disarm(now.unix_ms),
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

/// Clear `peer`'s cached hello capability — every refused-hello path in
/// [`dispatch_hello`] routes through here. PAN-96: when that erases a
/// still-live capability held by the CURRENT controller, treat the clear
/// exactly like expiry ([`expire_peer_auth`]): disarm and release control,
/// returning the updated `controlState` frames for the remaining peers.
/// Without this the lapse is invisible to the expiry sweep (the old `exp_ms`
/// is erased while still unexpired), while heartbeats — which bind only to
/// the arm's own jti — could keep the arm alive for a controller the station
/// can no longer send anything to.
fn clear_peer_auth(ctx: &mut ArmContext, peer: &str, now: i64) -> Vec<PeerSend> {
    let had_auth = ctx
        .peers
        .get_mut(peer)
        .is_some_and(|p| p.auth.take().is_some());
    if !had_auth || ctx.controller.as_deref() != Some(peer) {
        return Vec::new();
    }
    let effects = match ctx.arm.lock() {
        Ok(mut st) => st.disarm(now),
        Err(_) => Vec::new(),
    };
    if !effects.is_empty() {
        warn!(target: "agent.tx", peer = %peer,
            "controller capability cleared by a refused hello with a live arm — disarming");
    }
    apply_arm_effects(&ctx.audit, &effects);
    ctx.controller = None;
    // The clear path only has wall time (`now`); sample the session clock for
    // the `transmitArmed` broadcast — `control_state_sends` re-samples under
    // the arm lock anyway, and the loop's injected test clock stays coherent.
    control_state_sends(ctx, (ctx.clock)(), None)
}

/// Handle a `hello.capabilityToken` (Q-0019 #5): verify it against the pinned
/// IdP keys, bind it to the ACTUAL SENDING PEER's E2E-connected client
/// identity (`peer` — the value `MultiPeerSession`'s demux reported alongside
/// this specific frame, never a session-global value — since PAN-95 the
/// identity the peer PROVED in its Noise handshake), and set that peer's
/// `PeerCtx::auth` (scopes + expiry) accordingly. Fail-closed: any
/// missing/invalid/mismatched token clears the capability (the peer is sent
/// nothing and served nothing scoped), never partially trusts one.
///
/// `capability_token: None` (a legacy hello, v1 back-compat) also clears
/// scopes — the token is optional on the wire but required at runtime for any
/// scoped action.
///
/// Returns whether the peer now holds a verified capability, plus any
/// `controlState` frames a refusal must deliver: a refusal that erased a
/// capability the CURRENT controller still held disarms it and releases
/// control ([`clear_peer_auth`]) — the same treatment expiry gets.
///
/// SECURITY (multi-peer rebinding): the comparison is `cap.client_key_id ==
/// peer`, NOT against any field stored on `ctx` — `ctx` is shared across every
/// concurrently-connected peer, so a check against session-global state would
/// let peer B's hello (correctly Noise-authenticated as B by the demux) be
/// accepted under a capabilityToken that claims to be peer A, if A happened to
/// be the value some stale/shared field held. Binding to the per-frame `peer`
/// parameter is what keeps each peer's authorization strictly its own.
fn dispatch_hello(
    capability_token: Option<String>,
    peer: &str,
    ctx: &mut ArmContext,
    now: i64,
) -> (bool, Vec<PeerSend>) {
    let Some(token) = capability_token else {
        return (false, clear_peer_auth(ctx, peer, now));
    };
    match ctx.verifier.verify_capability_token(&token, now) {
        Ok(cap) if cap.client_key_id == peer && ctx.revoked_jtis.contains(&cap.jti) => {
            // PAN-96: a token deny-listed by an eviction never re-authorizes —
            // the client must fetch a fresh one.
            warn!(target: "agent.control", peer = %peer,
                "hello capabilityToken was revoked on this station — fetch a fresh token");
            (false, clear_peer_auth(ctx, peer, now))
        }
        Ok(cap) if cap.client_key_id == peer && !scope_at_least(&cap.scopes, "status") => {
            // A token with no `status`-or-higher scope (empty, or only
            // unrecognized strings) authorizes nothing the station can serve,
            // not even outbound frames. Storing it would leave a future-dated
            // capability that `peer_may_receive` refuses but the expiry sweep
            // never flags, so an armed controller would stay armed while sent
            // nothing. Treat it as a refused hello: clear, disarm, release.
            warn!(target: "agent.control", peer = %peer, scopes = ?cap.scopes,
                "hello capabilityToken carries no status-or-higher scope — no scope granted");
            (false, clear_peer_auth(ctx, peer, now))
        }
        Ok(cap) if cap.client_key_id == peer => {
            debug!(
                target: "agent.control",
                peer = %peer,
                scopes = ?cap.scopes,
                "hello capabilityToken verified — scopes granted for this peer"
            );
            match ctx.peers.get_mut(peer) {
                Some(p) => {
                    p.auth = Some(PeerAuth {
                        scopes: cap.scopes,
                        exp_ms: cap.exp_ms,
                        jti: cap.jti,
                    });
                    (true, Vec::new())
                }
                None => {
                    // Defensive: cannot happen via the demux (a Plaintext poll
                    // is only ever emitted for an established peer, and every
                    // established peer gets a PeerCtx on PeerEstablished), but
                    // fail closed rather than panic if it ever did.
                    warn!(target: "agent.control", peer = %peer, "hello verified for a peer with no PeerCtx — refusing");
                    (false, Vec::new())
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
            (false, clear_peer_auth(ctx, peer, now))
        }
        Err(e) => {
            warn!(target: "agent.control", peer = %peer, reason = %e, "hello capabilityToken invalid — no scope granted");
            (false, clear_peer_auth(ctx, peer, now))
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
    now: ArmTime,
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
        .verify_capability_token(capability_token, now.unix_ms)
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
            now.unix_ms,
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
            // Arming over this channel requires THIS channel's own consent
            // (`remote_tx_enabled`); another channel's consent never
            // satisfies it. Read under the same lock as the arm so a
            // concurrent consent change cannot interleave. `st.arm` refuses
            // again underneath (defense-in-depth).
            if !st.channel_consent(ConsentChannel::StationAgent) {
                return Err(
                    "station-agent remote TX consent is off (remote_tx_enabled=false)".to_string(),
                );
            }
            st.arm(verified, fresh_arm_time(ctx, now))
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
/// `peer` is the station-agent client keyId that requested this TX — stamped
/// onto the resulting QSO as `remote_client_key_id` so the arm gate can bind
/// TX permission to THIS specific client (PAN-91), not just "some client is
/// currently armed".
fn tx_kind_to_qso_message(kind: TxKind, peer: &str) -> crate::message_bus::QsoMessage {
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
            remote_client_key_id: Some(peer.to_string()),
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
            remote_client_key_id: Some(peer.to_string()),
        },
        TxKind::StartCq { offset_hz } => QsoMessage::StartCq {
            frequency: offset_hz.max(0.0) as u64,
            tx_parity: None,
            remote_origin: true,
            remote_client_key_id: Some(peer.to_string()),
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
    ///   `ServerEvent` and `MultiPeerSession::broadcast_to`s it to every
    ///   hello-authorized established peer over the relay socket — real off-box network traffic
    ///   carrying replayed decodes, spectrum, QSO state and dial frequency
    ///   fabricated as live.
    /// - INBOUND: an established peer's control frames (`dispatch_action` →
    ///   `send_rig`/`send_qso`) would otherwise drive QSY/QSO actions against a
    ///   demo process. Replay already skips Hamlib, so there is no PTT
    ///   capability, but a remote control surface has no business existing
    ///   during a demo at all.
    ///
    /// Local consent seeding (below, ahead of this check) still runs: it only
    /// mirrors config into the in-process arm's per-channel consent bits,
    /// touches nothing off-box, and keeps the arm's values identical to a live
    /// run's — with no session to admit a controller, it can never lead to TX.
    pub(crate) async fn start_station_agent_component(&mut self) -> Result<()> {
        let config = self.config.read().await;
        let cfg = config.network.station_agent.clone();
        let cqdx_cfg = config.network.cqdx.clone();
        // The shared remote-TX arm tracks consent PER CHANNEL: the
        // station-agent channel's bit is `remote_tx_enabled` alone, the WSJT-X
        // UDP channel's is `enabled && allow_tx_initiation`. Neither channel's
        // setting ever satisfies the other's gate.
        //
        // The wsjtx bit is gated on `enabled` too (mirrors the
        // coordinator-constructor seeding): with the component disabled there
        // is no Reply-handling path, so consent from `allow_tx_initiation`
        // alone is pure risk with no benefit.
        let wsjtx_allow_tx_initiation =
            config.network.wsjtx_udp.enabled && config.network.wsjtx_udp.allow_tx_initiation;
        drop(config);

        // Seed both channels' consent from config regardless of
        // enabled/paired. This mirrors (idempotently) the
        // coordinator-constructor seeding.
        {
            let now = now_ms();
            if let Ok(mut st) = self.remote_tx_arm.lock() {
                let _ = st.set_channel_consent(
                    ConsentChannel::StationAgent,
                    cfg.remote_tx_enabled,
                    now,
                );
                let _ = st.set_channel_consent(
                    ConsentChannel::WsjtxUdp,
                    wsjtx_allow_tx_initiation,
                    now,
                );
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
        // PAN-96: bumped by the poll task when an authorization refresh
        // changes the admission lists; the session loop reconciles on it (see
        // `ArmContext::authz_generation`).
        let authz_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));

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
            let poll_authz_generation = authz_generation.clone();
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
                    poll_authz_generation,
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
                authz_generation,
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
///
/// PAN-96: when a successful poll changes either list's contents, this task
/// bumps `authz_generation` after publishing them. The session loop notices
/// within one `RECV_TICK` and runs [`reconcile_authorizations`], which evicts
/// already-connected peers the new lists dropped and admits newly listed ones.
/// A poll that changes nothing, or fails, bumps nothing.
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
    authz_generation: Arc<std::sync::atomic::AtomicU64>,
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
                let arm_changed;
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
                    let mut g = tx_arm_allow_list.write().unwrap();
                    arm_changed = *g != new_tx_arm_allow;
                    *g = new_tx_arm_allow;
                }

                let allow_changed = {
                    let mut g = tx_allow_list.write().unwrap();
                    let changed = *g != new_allow;
                    *g = new_allow;
                    changed
                };
                *client_keys.write().unwrap() = new_keys;
                if arm_changed || allow_changed {
                    // PAN-96: bumped AFTER publishing, so a session that
                    // observes the new generation reads the new lists.
                    authz_generation.fetch_add(1, Ordering::AcqRel);
                }
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
    /// Threaded into `ArmContext::authz_generation` — see there.
    authz_generation: Arc<std::sync::atomic::AtomicU64>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

/// The connect → authenticate → process → (disarm on loss) → reconnect loop.
///
/// On any session teardown the arm is disarmed (fail TX-off on control-channel
/// loss, Part-97), then the loop reconnects with capped backoff until shutdown.
async fn run_session_loop(mut cfg: RunConfig) {
    let mut disarm_guard = RemoteTxDisarmGuard {
        arm: cfg.arm.clone(),
        audit: cfg.audit.clone(),
        shutdown: cfg.shutdown.clone(),
        normal_exit: false,
    };
    let mut backoff = RECONNECT_BACKOFF_MIN;
    let mut ctx = ArmContext {
        arm: cfg.arm.clone(),
        verifier: cfg.verifier,
        client_keys: cfg.client_keys,
        tx_allow_list: cfg.tx_allow_list,
        tx_arm_allow_list: cfg.tx_arm_allow_list,
        // Starts empty; `reconcile_authorizations` adds the hello-token jti of
        // every peer an authorization refresh evicts (PAN-96). It lives across
        // reconnects for the rest of this task's lifetime. Future seam: a cqdx
        // revocation feed could populate it too.
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
        authz_generation: cfg.authz_generation,
        // Set from the live generation at the start of every session.
        authz_generation_seen: 0,
        clock: Arc::new(ArmTime::now),
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
    disarm_guard.mark_normal_exit();
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
/// loop calls after every dispatch (and on peer-down / revocation). A frame
/// addressed to a peer [`peer_may_receive`] refuses is withheld. The clock is
/// read for each frame, since earlier sends can block across token expiry.
/// (PAN-95). A failed serialize or send is `debug!`-logged and skipped
/// (best-effort — a control frame that can't reach one peer never tears down
/// the session).
fn deliver_sends<W: pancetta_agent::relay::WsConn>(
    sess: &mut MultiPeerSession<'_, W>,
    ctx: &ArmContext,
    sends: &[PeerSend],
    clock: impl Fn() -> i64,
) {
    for send in sends {
        let SendTarget::One(peer) = &send.to;
        if !peer_may_receive(ctx, peer, clock()) {
            debug!(target: "agent.control", peer = %peer, "frame withheld — peer holds no valid hello capability");
            continue;
        }
        let bytes = match serde_json::to_vec(&send.frame) {
            Ok(b) => b,
            Err(e) => {
                debug!(target: "agent.control", "controlState serialize failed: {e}");
                continue;
            }
        };
        if let Err(e) = sess.send_to(peer, &bytes) {
            debug!(target: "agent.control", peer = %peer, "peer send failed: {e}");
        }
    }
}

/// Most display events (or lag notices) [`drain_read_stream`] fans out per
/// call. A display producer that never pauses must still hand control back to
/// the session loop — and its arm-expiry sweep — on every iteration (PAN-97).
const READ_STREAM_DRAIN_MAX: usize = 64;

/// Fan pending display events out to every established peer that
/// [`peer_may_receive`] admits, reading the clock for each recipient check
/// (PAN-95: a peer without an unexpired `status`+ hello capability gets
/// nothing). Each call consumes at most [`READ_STREAM_DRAIN_MAX`] events or
/// lag notifications before returning to session housekeeping (PAN-97: so a
/// busy read stream cannot hold the arm-expiry sweep off). Lossy by design:
/// Lagged means the bounded broadcast ring dropped old events; log and
/// continue within the same batch limit.
fn drain_read_stream<W: pancetta_agent::relay::WsConn>(
    events: &mut Option<tokio::sync::broadcast::Receiver<pancetta_protocol::ServerEvent>>,
    sess: &mut MultiPeerSession<'_, W>,
    ctx: &ArmContext,
    clock: impl Fn() -> i64,
) {
    let Some(rx) = events.as_mut() else { return };
    // Bound work even when producers keep the receiver nonempty, so the
    // session loop returns to authorization, controller-expiry and arm-expiry
    // housekeeping.
    for _ in 0..READ_STREAM_DRAIN_MAX {
        match rx.try_recv() {
            Ok(ev) => {
                let frame = pancetta_protocol::ServerFrame::Event { event: ev };
                match serde_json::to_vec(&frame) {
                    Ok(bytes) => {
                        let _ = sess.broadcast_to(&bytes, |p| peer_may_receive(ctx, p, clock()));
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
/// granted scope carries across a reconnect. Within a session, the demux's
/// admission set is refreshed on every authorization-generation change
/// (PAN-96, [`reconcile_authorizations`]): a peer the refresh dropped is
/// evicted at once, and a newly listed one may connect without a reconnect.
async fn run_one_session<W: pancetta_agent::relay::WsConn>(
    ws: W,
    identity: &AgentIdentity,
    ctx: &mut ArmContext,
    bus: &MessageBus,
    events: &mut Option<tokio::sync::broadcast::Receiver<pancetta_protocol::ServerEvent>>,
) {
    ctx.peers.clear();
    // D8 (PAN-96): record the generation BEFORE snapshotting the list. A poll
    // that publishes after the snapshot bumps afterwards and triggers a
    // reconcile; one landing between the two costs one harmless extra one.
    ctx.authz_generation_seen = ctx.authz_generation.load(Ordering::Acquire);
    let allow_snapshot = ctx.tx_allow_list.read().unwrap().clone();
    let mut sess = MultiPeerSession::new(ws, identity, allow_snapshot);
    if let Err(e) = sess.authenticate() {
        debug!(target: "agent", "relay authenticate failed: {e}");
        return;
    }
    loop {
        let polled = sess.process_next(RECV_TICK);
        // PAN-97: expire a lapsed arm on EVERY iteration, before acting on
        // what was received — time spent receiving counts, and no branch below
        // (valid traffic, malformed frames, idle ticks) can skip it. The
        // TX worker's key-time gate still checks the clock independently.
        let now = (ctx.clock)();
        let expiry_sends = sweep_arm_expiry(ctx, now);
        deliver_sends(&mut sess, ctx, &expiry_sends, || (ctx.clock)().unix_ms);
        // That delivery can block on a backed-up relay socket, and a hello or
        // arm capability may expire meanwhile: authorize what was received at
        // a fresh instant, never the pre-delivery one.
        let now = fresh_arm_time(ctx, now);
        match polled {
            Ok(Poll::Plaintext { peer, plaintext }) => {
                let action = match map_client_frame(&plaintext) {
                    Ok(a) => a,
                    Err(e) => {
                        debug!(target: "agent.control", "malformed control frame: {e}");
                        // PAN-34: this `continue` must not skip the
                        // end-of-loop housekeeping below (authorization
                        // reconcile, revocation broadcast, expiry sweep), or
                        // a client that keeps resending bad frames can
                        // suppress it for every OTHER connected peer
                        // indefinitely.
                        session_housekeeping(ctx, &mut sess);
                        continue;
                    }
                };
                // `sid` is THIS SPECIFIC PEER's own channel-binding session id
                // (never a session-global value) — the exact input
                // `verify_and_arm`/`verify_arm_grant`'s sessionId bind needs to
                // block a grant captured on one peer's session from being
                // replayed into another peer's.
                let sid = sess.session_id(&peer).unwrap_or_default().to_string();
                // A receive can cross expiry. Retire the previous authority
                // before a fresh hello can replace it and hide the lapse.
                let mut sends = expire_peer_auth(ctx, now);
                let outcome = dispatch_action(action, &peer, ctx, bus, &sid, now).await;
                sends.extend(outcome.sends);
                deliver_sends(&mut sess, ctx, &sends, || (ctx.clock)().unix_ms);
                // Task 6: drain the read stream after every successful dispatch
                // too, not just on Quiet/Idle — a chatty control session
                // (frequent Arm/Heartbeat/Qsy traffic) must never starve the
                // display feed of a chance to fan out.
                drain_read_stream(events, &mut sess, ctx, || (ctx.clock)().unix_ms);
                if outcome.flow == Dispatch::Teardown {
                    return;
                }
            }
            Ok(Poll::PeerEstablished { peer, .. }) => {
                // The peer's identity is proven, but it holds no capability
                // yet: it is sent nothing. The Rule-7 controlState greeting
                // is sent on its first valid `hello` (PAN-95, D5).
                ctx.peers.insert(peer, PeerCtx::unauthorized());
            }
            Ok(Poll::PeerRefused { peer, reason }) => {
                warn!(target: "agent", peer = %peer, reason = ?reason, "remote peer refused");
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
                        Ok(mut st) => st.disarm(now.unix_ms),
                        Err(_) => Vec::new(),
                    };
                    if !effects.is_empty() {
                        debug!(target: "agent.tx", peer = %peer, "controller left with a live arm — disarming");
                    }
                    apply_arm_effects(&ctx.audit, &effects);
                    ctx.controller = None;
                    let sends = control_state_sends(ctx, now, None);
                    deliver_sends(&mut sess, ctx, &sends, || (ctx.clock)().unix_ms);
                }
            }
            Ok(Poll::Idle) | Ok(Poll::Quiet) => {
                // Task 6: the common case — no control traffic this tick, so
                // this is where the read stream gets most of its chances to
                // fan out pending display events to established peers.
                drain_read_stream(events, &mut sess, ctx, || (ctx.clock)().unix_ms);
            }
            Ok(Poll::Closed) => return,
            Err(e) => {
                debug!(target: "agent", "session error: {e}");
                return;
            }
        }

        // Round-1 review fix (PR #261), broadcast now factored into
        // `session_housekeeping` (PAN-34, PAN-96) so the malformed-frame
        // `continue` above can't bypass it — see that fn's doc comment.
        session_housekeeping(ctx, &mut sess);
    }
}

/// Per-tick housekeeping, run at the end of every loop iteration AND on the
/// malformed-frame `continue` path (PAN-34: a misbehaving peer must not be
/// able to suppress it for the others). Applies any authorization refresh
/// first (PAN-96, [`reconcile_authorizations`]), so the poll-flagged
/// rebroadcast and the expiry sweep both run on the post-eviction peer set.
fn session_housekeeping<W: pancetta_agent::relay::WsConn>(
    ctx: &mut ArmContext,
    sess: &mut MultiPeerSession<'_, W>,
) {
    let now = (ctx.clock)();
    let sends = reconcile_authorizations(ctx, sess, now);
    deliver_sends(sess, ctx, &sends, || (ctx.clock)().unix_ms);
    broadcast_pending_revocation(ctx, sess, now);
    // The deliveries above can block; check capability expiry at a fresh
    // instant so an expired controller is released this tick.
    let now = fresh_arm_time(ctx, now);
    let sends = expire_peer_auth(ctx, now);
    deliver_sends(sess, ctx, &sends, || (ctx.clock)().unix_ms);
}

/// Broadcast the current (already-disarmed) control state to every
/// established peer if a concurrent `poll_authorizations_loop` cycle flagged
/// `ctx.revocation_pending` since the last check. Called from
/// [`session_housekeeping`], which both the normal end-of-tick path AND the
/// malformed-control-frame `continue` (PAN-34) run — a persistently
/// misbehaving client must not be able to suppress the broadcast to other
/// connected peers by never reaching the bottom of the loop.
fn broadcast_pending_revocation<W: pancetta_agent::relay::WsConn>(
    ctx: &ArmContext,
    sess: &mut MultiPeerSession<'_, W>,
    now: ArmTime,
) {
    if ctx.revocation_pending.swap(false, Ordering::AcqRel) {
        let sends = control_state_sends(ctx, now, None);
        deliver_sends(sess, ctx, &sends, || (ctx.clock)().unix_ms);
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
    /// The real pairing.v1 keyId of [`client_device`] (Ed25519 seed 0x22 —
    /// the same key as [`client_key`]), so tokens, grants and `clientSig`
    /// fixtures stay coherent with an identity a handshake can PROVE (PAN-95).
    /// Pinned by `test_device_constants_match_their_keys`.
    const CLIENT_KEY_ID: &str = "dE82zKZ-sZEssoLgjoa9L-DQkASijPbnxr1Wzda_Zao";
    const IDP_KID: &str = "idp-kid-1";
    const OPERATOR: &str = "K5ARH";
    const NOW: i64 = 1_700_000_000_000;

    /// One shared monotonic origin for every test timeline.
    fn mono_base() -> std::time::Instant {
        static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(std::time::Instant::now)
    }

    /// Every arm clock at `ms`: wall = `ms` unix ms, monotonic = origin + `ms`,
    /// suspend-aware = `ms`.
    fn at(ms: i64) -> ArmTime {
        ArmTime {
            unix_ms: ms,
            monotonic: mono_base() + Duration::from_millis(ms.unsigned_abs()),
            suspend_aware_ms: ms,
        }
    }

    /// A session clock pinned at `ms`.
    fn fixed_clock(ms: i64) -> ArmClock {
        Arc::new(move || at(ms))
    }

    /// A session clock that reads `ms` on every call, so a test can advance
    /// the loop's timeline between scripted steps.
    fn manual_clock(ms: &Arc<std::sync::atomic::AtomicI64>) -> ArmClock {
        let ms = ms.clone();
        Arc::new(move || at(ms.load(Ordering::SeqCst)))
    }
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

    impl PeerCtx {
        /// Test helper: an established peer holding `scopes` until `exp_ms`.
        fn authorized_until(scopes: Vec<String>, exp_ms: i64) -> Self {
            Self {
                auth: Some(PeerAuth {
                    scopes,
                    exp_ms,
                    jti: "test-jti".to_string(),
                }),
            }
        }
    }

    /// A `peers` map with a single established entry for `peer`. `Some(scopes)`
    /// is a verified hello valid until `NOW + 600_000` (matching
    /// `valid_token()`'s `exp`); `None` is an established, unauthorized peer.
    fn peers_with(
        peer: &str,
        hello_scopes: Option<Vec<String>>,
    ) -> std::collections::HashMap<String, PeerCtx> {
        let mut m = std::collections::HashMap::new();
        let ctx = match hello_scopes {
            Some(scopes) => PeerCtx::authorized_until(scopes, NOW + 600_000),
            None => PeerCtx::unauthorized(),
        };
        m.insert(peer.to_string(), ctx);
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
            authz_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            authz_generation_seen: 0,
            clock: fixed_clock(NOW),
        }
    }

    fn audit_tmp() -> std::path::PathBuf {
        use std::sync::atomic::AtomicU64;
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("pancetta-sa-test-{}-{n}.log", std::process::id()))
    }

    fn with_channel_consent(ctx: &ArmContext, channel: ConsentChannel, on: bool) {
        ctx.arm
            .lock()
            .unwrap()
            .set_channel_consent(channel, on, NOW);
    }

    /// An `ArmState` that is currently armed, so `RemoteTxDisarmGuard`'s
    /// `Drop` has something to disarm.
    fn armed_state() -> ArmState {
        let mut st = ArmState::new();
        st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
        st.arm(
            pancetta_agent::arm::VerifiedArmGrant {
                operator_callsign: OPERATOR.to_string(),
                ttl_ms: 300_000,
                armed_until_ms: NOW + 300_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: "test-jti".to_string(),
                client_key_id: CLIENT_KEY_ID.to_string(),
            },
            at(NOW),
        );
        st
    }

    /// PAN-19 MEDIUM #4: `RemoteTxDisarmGuard::drop` used to attribute
    /// EVERY disarm to `DisarmReason::ComponentCrash`, unconditionally --
    /// including on an ordinary, requested shutdown (`run_session_loop`
    /// returning normally, not via panic/unwind). Simulate that: construct
    /// the guard with `normal_exit` already set and the arm still armed (as
    /// it could be if `disarm_on_loss`'s own prior disarm silently gave up
    /// on a poisoned mutex), drop it, and confirm the audit log attributes
    /// the disarm to the shutdown path, not a crash.
    #[test]
    fn drop_during_ordinary_shutdown_does_not_audit_a_component_crash() {
        let arm = Arc::new(Mutex::new(armed_state()));
        let audit = AuditLog::new(audit_tmp());

        {
            let _guard = RemoteTxDisarmGuard {
                arm: arm.clone(),
                audit: audit.clone(),
                shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                normal_exit: true,
            };
        }

        let log = std::fs::read_to_string(audit.path()).unwrap_or_default();
        assert!(
            !log.contains("component-crash"),
            "an ordinary shutdown must never be audited as a component crash: {log:?}"
        );
        assert!(
            log.contains("operator-disarm"),
            "expected the shutdown-path disarm to be attributed to operator-disarm: {log:?}"
        );
    }

    /// The flip side of the test above, updated for PAN-36 round 2: a drop
    /// with `normal_exit` unset is not on its own proof of a crash --
    /// `shutdown.rs` force-`abort()`s a task that overran its 1s grace
    /// period, which drops this guard via ordinary (non-unwinding) cleanup,
    /// same as a ordinary drop. When shutdown was actually requested (round
    /// 3: the guard's own `shutdown` flag is true), that must still audit as
    /// `OperatorDisarm`, not `ComponentCrash` -- a genuine panic is covered
    /// separately below (`drop_during_genuine_panic_...`), which actually
    /// unwinds; a non-panicking drop with shutdown NOT requested is covered
    /// by `drop_without_shutdown_requested_and_not_panicking_audits_a_crash`.
    #[test]
    fn drop_without_normal_exit_but_not_panicking_still_audits_operator_disarm() {
        let arm = Arc::new(Mutex::new(armed_state()));
        let audit = AuditLog::new(audit_tmp());

        {
            let _guard = RemoteTxDisarmGuard {
                arm: arm.clone(),
                audit: audit.clone(),
                shutdown: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                normal_exit: false,
            };
            // Guard drops here via ordinary scope exit -- simulating a
            // shutdown.rs `abort()`-triggered cancellation, not a panic,
            // with shutdown actually having been requested.
        }

        let log = std::fs::read_to_string(audit.path()).unwrap_or_default();
        assert!(
            !log.contains("component-crash"),
            "a non-panicking drop during requested shutdown (e.g. shutdown.rs's \
             abort-on-timeout) must never be misattributed as a component crash: {log:?}"
        );
        assert!(
            log.contains("operator-disarm"),
            "expected a non-panicking, non-normal-exit drop during requested shutdown to still \
             audit as operator-disarm: {log:?}"
        );
    }

    /// PAN-36 round 3 (Codex finding on the round-2 fix): treating every
    /// non-panicking drop as automatically operator-initiated is too broad
    /// -- a cancellation unrelated to shutdown (a hypothetical future
    /// `AbortHandle` call, a runtime-level drop) would also read as "not
    /// panicking" and get misattributed as `OperatorDisarm`, hiding a real
    /// unexpected termination. With `normal_exit` unset AND the shutdown
    /// flag never having been set, a non-panicking drop must fall back to
    /// `ComponentCrash`, not be assumed benign.
    #[test]
    fn drop_without_shutdown_requested_and_not_panicking_audits_a_crash() {
        let arm = Arc::new(Mutex::new(armed_state()));
        let audit = AuditLog::new(audit_tmp());

        {
            let _guard = RemoteTxDisarmGuard {
                arm: arm.clone(),
                audit: audit.clone(),
                shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                normal_exit: false,
            };
            // Guard drops here via ordinary scope exit -- not a panic, but
            // shutdown was never requested either (e.g. a hypothetical
            // future non-shutdown cancellation of this task).
        }

        let log = std::fs::read_to_string(audit.path()).unwrap_or_default();
        assert!(
            log.contains("component-crash"),
            "a non-panicking drop with shutdown never requested must not be assumed to be an \
             ordinary operator action: {log:?}"
        );
        assert!(
            !log.contains("operator-disarm"),
            "a non-panicking, non-shutdown-requested drop must not be misattributed to \
             operator-disarm: {log:?}"
        );
    }

    /// The scenario `RemoteTxDisarmGuard` exists for in the first place: a
    /// real panic/unwind, with `normal_exit` never reached. Drives an actual
    /// panic through `catch_unwind` so `std::thread::panicking()` is
    /// genuinely true while the guard drops, unlike the ordinary-drop test
    /// above.
    #[test]
    fn drop_during_genuine_panic_audits_a_component_crash() {
        let arm = Arc::new(Mutex::new(armed_state()));
        let audit = AuditLog::new(audit_tmp());
        let arm_for_panic = arm.clone();
        let audit_for_panic = audit.clone();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = RemoteTxDisarmGuard {
                arm: arm_for_panic,
                audit: audit_for_panic,
                shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                normal_exit: false,
            };
            panic!("simulated genuine task panic");
        }));
        assert!(result.is_err(), "the closure must have actually panicked");

        let log = std::fs::read_to_string(audit.path()).unwrap_or_default();
        assert!(
            log.contains("component-crash"),
            "a disarm during a genuine panic/unwind must still be attributed to a component \
             crash: {log:?}"
        );
    }

    /// PAN-36 (round-18 finding on PR #254, still relevant after round 3
    /// reintroduced a `shutdown` field to the guard): the shared `shutdown`
    /// flag can be set true by a completely unrelated task at any moment --
    /// including the exact instant this task's session loop panics for a
    /// real, unrelated reason. That coincidence used to misattribute a
    /// genuine crash as `OperatorDisarm`. Simulate the race directly: the
    /// guard's own `shutdown` flag is already true (as if shutdown was
    /// requested around the same time), but this guard's own `normal_exit`
    /// was never set because the panic happened before `run_session_loop`
    /// reached its own clean-return path. Because `Drop` checks
    /// `!std::thread::panicking()` FIRST, a real panic must still win over
    /// the shutdown flag and be attributed to `ComponentCrash`.
    #[test]
    fn drop_during_genuine_panic_with_concurrent_shutdown_flag_still_audits_a_component_crash() {
        let arm = Arc::new(Mutex::new(armed_state()));
        let audit = AuditLog::new(audit_tmp());
        // Shutdown requested at (or just before) the exact moment this
        // task's own, unrelated panic unwinds -- must not mask the panic.
        let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let arm_for_panic = arm.clone();
        let audit_for_panic = audit.clone();
        let shutdown_for_panic = shutdown_flag.clone();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = RemoteTxDisarmGuard {
                arm: arm_for_panic,
                audit: audit_for_panic,
                shutdown: shutdown_for_panic,
                normal_exit: false,
            };
            // A real panic, so the guard drops while genuinely unwinding --
            // the shutdown flag being true must have no bearing.
            panic!("simulated genuine task panic racing a concurrent shutdown flag");
        }));
        assert!(result.is_err(), "the closure must have actually panicked");

        let log = std::fs::read_to_string(audit.path()).unwrap_or_default();
        assert!(
            log.contains("component-crash"),
            "a genuine panic must be attributed to a component crash even when the shutdown \
             flag happens to be set at the same instant: {log:?}"
        );
        assert!(
            !log.contains("operator-disarm"),
            "a genuine panic must never be misattributed to operator-disarm just because a \
             concurrent shutdown flag happens to be true: {log:?}"
        );
    }

    /// PAN-36 round 2 (Codex finding on this PR): reproduces the actual
    /// shutdown.rs mechanism end-to-end rather than just the `Drop` logic in
    /// isolation. `run_one_session` never checks `cfg.shutdown` itself, so a
    /// session with an established peer can still be mid-flight when
    /// shutdown is requested; `shutdown.rs` gives the task a 1s grace
    /// period, then force-`abort()`s it -- dropping this guard before
    /// `mark_normal_exit` is ever reached. With shutdown actually requested
    /// (round 3: the guard's own `shutdown` flag set beforehand, matching
    /// how `shutdown.rs` sets it before its abort loop runs), that must
    /// audit as `OperatorDisarm`, not `ComponentCrash`.
    #[tokio::test]
    async fn task_abort_mid_session_drops_the_guard_as_operator_disarm_not_a_crash() {
        let arm = Arc::new(Mutex::new(armed_state()));
        let audit = AuditLog::new(audit_tmp());
        let arm_for_task = arm.clone();
        let audit_for_task = audit.clone();
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let shutdown_for_task = shutdown.clone();

        let handle = tokio::spawn(async move {
            let _guard = RemoteTxDisarmGuard {
                arm: arm_for_task,
                audit: audit_for_task,
                shutdown: shutdown_for_task,
                normal_exit: false,
            };
            // Stand-in for `run_one_session` still mid-flight (e.g. awaiting
            // a peer message) when shutdown.rs's grace period elapses.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });

        // Let the task actually start and construct the guard before
        // aborting it, mirroring shutdown.rs's abort-after-timeout shape.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        handle.abort();
        let _ = handle.await;

        let log = std::fs::read_to_string(audit.path()).unwrap_or_default();
        assert!(
            !log.contains("component-crash"),
            "a shutdown.rs-style task abort with shutdown requested must never be misattributed \
             as a component crash: {log:?}"
        );
        assert!(
            log.contains("operator-disarm"),
            "expected the abort-triggered drop during requested shutdown to audit as \
             operator-disarm: {log:?}"
        );
    }

    /// PAN-36 round 3 companion to the test above: an abort that happens
    /// WITHOUT shutdown ever having been requested (a hypothetical future
    /// non-shutdown cancellation of this task) must NOT be assumed to be an
    /// ordinary operator action -- it should still audit as `ComponentCrash`
    /// so an unexpected termination isn't hidden from the audit log.
    #[tokio::test]
    async fn task_abort_without_shutdown_requested_still_audits_a_component_crash() {
        let arm = Arc::new(Mutex::new(armed_state()));
        let audit = AuditLog::new(audit_tmp());
        let arm_for_task = arm.clone();
        let audit_for_task = audit.clone();
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_for_task = shutdown.clone();

        let handle = tokio::spawn(async move {
            let _guard = RemoteTxDisarmGuard {
                arm: arm_for_task,
                audit: audit_for_task,
                shutdown: shutdown_for_task,
                normal_exit: false,
            };
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        handle.abort();
        let _ = handle.await;

        let log = std::fs::read_to_string(audit.path()).unwrap_or_default();
        assert!(
            log.contains("component-crash"),
            "an abort-triggered drop with shutdown never requested must not be assumed to be an \
             ordinary operator action: {log:?}"
        );
        assert!(
            !log.contains("operator-disarm"),
            "an abort-triggered drop without requested shutdown must not be misattributed to \
             operator-disarm: {log:?}"
        );
    }

    // ── Case 2: a validly-signed Arm from an allow-listed client permits TX ──
    #[tokio::test]
    async fn arm_from_allowlisted_client_permits_tx() {
        let mut ctx = ctx_with(true, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        let d = dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW), ConsentChannel::StationAgent),
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
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        let d = dispatch_action(
            arm_action("arm-jti-owner"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW), ConsentChannel::StationAgent),
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
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        let d = dispatch_action(
            arm_action("arm-jti-delegate"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            !ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW), ConsentChannel::StationAgent),
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
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        // `arm_action`'s grant carries `sessionId: "sess-1"` (== SESSION_ID);
        // dispatch it against a DIFFERENT live session id.
        let d = dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            "a-completely-different-session",
            at(NOW),
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            !ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW), ConsentChannel::StationAgent),
            "a grant signed for a different session must never arm this one"
        );
    }

    // ── Q-0019 #5: hello.capabilityToken roots read/qsy authorization ───────

    #[tokio::test]
    async fn qsy_refused_without_prior_hello_token() {
        // No hello has been dispatched this session — the peer's auth is None
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
            at(NOW),
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
            at(NOW),
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
            at(NOW),
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
            at(NOW),
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
            at(NOW),
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
        ctx.tx_allow_list
            .write()
            .unwrap()
            .insert(ANOTHER_PEER.to_string());
        ctx.peers
            .insert(ANOTHER_PEER.to_string(), PeerCtx::unauthorized());
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
            at(NOW),
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
            at(NOW),
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
            at(NOW),
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
            at(NOW),
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
        // fresh `PeerCtx::unauthorized()` inserted on the next
        // `PeerEstablished`) is exercised indirectly here: simulate a stale
        // prior grant by setting the peer's auth directly, then confirm
        // the field is what gates dispatch (i.e. a reconnect-boundary reset to
        // a fresh `PeerCtx` really does deny qsy, proving the gate reads
        // `PeerCtx::auth` and nothing else).
        let mut ctx = ctx_with(true, true);
        ctx.peers.insert(
            CLIENT_KEY_ID.to_string(),
            PeerCtx::authorized_until(vec!["qsy".to_string()], NOW + 600_000),
        );
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        // Simulate the reconnect-boundary reset a fresh `run_one_session`
        // performs: `ctx.peers.clear()` then a brand-new `PeerCtx` on the next
        // `PeerEstablished` — same peer keyId, but no scope carried over.
        ctx.peers
            .insert(CLIENT_KEY_ID.to_string(), PeerCtx::unauthorized());
        let d = dispatch_action(
            ControlAction::Qsy {
                vfo: 0,
                frequency_hz: 14_074_000.0,
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
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
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(NOW), ConsentChannel::StationAgent));
        // No further heartbeats: at the dead-man deadline, tx_permitted is false.
        let dead = NOW + HEARTBEAT_TIMEOUT_MS;
        assert!(
            !ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(dead), ConsentChannel::StationAgent),
            "no heartbeat within the window must auto-deny (dead-man)"
        );
        // A heartbeat *before* the deadline slides the window.
        let mut ctx2 = ctx_with(true, true);
        with_channel_consent(&ctx2, ConsentChannel::StationAgent, true);
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx2,
            &bus,
            SESSION_ID,
            at(NOW),
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
            at(NOW + 20_000),
        )
        .await;
        assert!(
            ctx2.arm.lock().unwrap().tx_permitted(
                at(NOW + 20_000 + HEARTBEAT_TIMEOUT_MS - 1),
                ConsentChannel::StationAgent
            ),
            "a heartbeat must slide the dead-man window"
        );
    }

    // ── A stale-seq Heartbeat does NOT extend the arm (replay can't hold open) ─
    #[tokio::test]
    async fn stale_seq_heartbeat_does_not_extend_the_arm() {
        let mut ctx = ctx_with(true, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        // Arm at NOW; the grant's jti is "arm-jti-1" (signed_grant uses it).
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(NOW), ConsentChannel::StationAgent));

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
            at(accepted_at),
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
            at(deadline - 1),
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
            at(deadline - 1),
        )
        .await;

        // Dead-man expires on schedule despite the later replays.
        assert!(ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(deadline - 1), ConsentChannel::StationAgent));
        assert!(
            !ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(deadline), ConsentChannel::StationAgent),
            "a replayed-seq heartbeat must NOT hold the arm open past the dead-man deadline"
        );
    }

    // ── A wrong-arm_jti Heartbeat is rejected and never extends the arm ──────
    #[tokio::test]
    async fn wrong_arm_jti_heartbeat_is_rejected() {
        let mut ctx = ctx_with(true, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
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
            at(NOW + 10_000),
        )
        .await;
        // Original dead-man deadline (from the arm at NOW) still governs.
        assert!(ctx.arm.lock().unwrap().tx_permitted(
            at(NOW + HEARTBEAT_TIMEOUT_MS - 1),
            ConsentChannel::StationAgent
        ));
        assert!(!ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(NOW + HEARTBEAT_TIMEOUT_MS), ConsentChannel::StationAgent));
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
            at(NOW),
        )
        .await;
        assert!(
            !ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW), ConsentChannel::StationAgent),
            "consent OFF must deny TX even after a valid arm"
        );
    }

    // ── PAN-98: each channel's consent gates only its own channel ───────────

    /// The ticket's Gherkin as a permanent regression test. Given WSJT-X UDP
    /// is enabled with `allow_tx_initiation=true` (wsjtx channel consent ON)
    /// and the station-agent's own `remote_tx_enabled` is false (station-agent
    /// channel consent OFF), when a TX-eligible client presents a valid
    /// capability and signed grant over the station-agent channel, then the
    /// arm is refused.
    #[tokio::test]
    async fn udp_consent_does_not_enable_station_agent_arm() {
        let mut ctx = ctx_with(true, true);
        // Mirror production seeding for remote_tx_enabled=false +
        // wsjtx_udp.enabled=true + allow_tx_initiation=true.
        with_channel_consent(&ctx, ConsentChannel::StationAgent, false);
        with_channel_consent(&ctx, ConsentChannel::WsjtxUdp, true);
        let bus = MessageBus::new(64).unwrap();
        let d = dispatch_action(
            arm_action("arm-jti-pan98"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        // The session continues; the ARM is refused.
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "the arm must be refused when the station-agent's own remote_tx_enabled is \
             false, even though wsjtx allow_tx_initiation is true"
        );
        assert!(!ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(NOW), ConsentChannel::StationAgent));
        assert!(!ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(NOW), ConsentChannel::WsjtxUdp));
        // The refusal is audited as a TxDenied naming the station-agent
        // channel's consent.
        let log = std::fs::read_to_string(ctx.audit.path()).unwrap_or_default();
        assert!(
            log.contains("arm rejected: station-agent remote TX consent is off"),
            "refusal must be audited: {log}"
        );
        // Every controlState sent reports transmitArmed: false.
        for send in &d.sends {
            if let pancetta_protocol::ServerFrame::Event {
                event: pancetta_protocol::ServerEvent::ControlState { transmit_armed, .. },
            } = &send.frame
            {
                assert!(!transmit_armed, "no controlState may claim an arm");
            }
        }
    }

    /// Intent acceptance: a TxRequest over the station-agent channel is
    /// refused (no StartQso routed, audited TxDenied) while that channel's own
    /// consent is off — even with the wsjtx channel's consent on.
    #[tokio::test]
    async fn tx_request_refused_when_station_agent_consent_off() {
        let mut ctx = ctx_with(true, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, false);
        with_channel_consent(&ctx, ConsentChannel::WsjtxUdp, true);
        let bus = MessageBus::new(64).unwrap();
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
            at(NOW),
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            qso_rx.try_recv().is_err(),
            "no StartQso may be routed when the station-agent channel's own consent is off"
        );
        let log = std::fs::read_to_string(ctx.audit.path()).unwrap_or_default();
        assert!(
            log.contains("txRequest refused: station-agent remote TX consent off"),
            "refusal must be audited as TxDenied: {log}"
        );
        let kinds: Vec<AuditKind> = log
            .lines()
            .map(|l| serde_json::from_str::<AuditEvent>(l).unwrap().kind)
            .collect();
        assert!(
            kinds.contains(&AuditKind::TxDenied) && !kinds.contains(&AuditKind::TxRequested),
            "a refused request is audited TxDenied, never TxRequested: {log}"
        );
    }

    /// No over-blocking: with the station-agent channel's own consent ON and
    /// the wsjtx channel's OFF, arm and TxRequest both still work.
    #[tokio::test]
    async fn arm_and_tx_request_still_work_when_own_consent_on_and_wsjtx_off() {
        let mut ctx = ctx_with(true, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        with_channel_consent(&ctx, ConsentChannel::WsjtxUdp, false);
        let bus = MessageBus::new(64).unwrap();
        let (_qso_tx, qso_rx) = bus.create_channel(ComponentId::Qso).await.unwrap();
        let d = dispatch_action(
            arm_action("arm-jti-pan98-ok"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(NOW), ConsentChannel::StationAgent));
        assert!(
            !ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW), ConsentChannel::WsjtxUdp),
            "the station-agent channel's consent never satisfies the wsjtx channel"
        );
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
            at(NOW),
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            qso_rx.try_recv().is_ok(),
            "StartQso must be routed when the station-agent channel's own consent is on"
        );
    }

    // ── Case 6: an Arm from a client NOT in the allow-list is rejected ──────
    #[tokio::test]
    async fn arm_from_unallowlisted_client_is_rejected() {
        // Client key present as a device key, but NOT in the allow-list.
        let mut ctx = ctx_with(false, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "a grant from a non-allow-listed client must NOT arm"
        );
        assert!(!ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(NOW), ConsentChannel::StationAgent));
    }

    // ── Explicit Disarm clears a live arm ───────────────────────────────────
    #[tokio::test]
    async fn explicit_disarm_clears_arm() {
        let mut ctx = ctx_with(true, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(NOW), ConsentChannel::StationAgent));
        dispatch_action(
            ControlAction::Disarm {
                arm_jti: "arm-jti-1".into(),
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
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
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
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
            at(NOW),
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
            at(NOW),
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
            at(NOW),
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
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
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
            at(NOW),
        )
        .await;
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "a token without txEnabledUntil must NEVER arm (clock-2 gate)"
        );
        assert!(!ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(NOW), ConsentChannel::StationAgent));
    }

    // ── txDisarm{armJti} disarms a live arm (armJti is a sanity match) ──────
    #[tokio::test]
    async fn tx_disarm_with_arm_jti_disarms() {
        let mut ctx = ctx_with(true, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(NOW), ConsentChannel::StationAgent));
        // A txDisarm naming the live arm disarms it.
        dispatch_action(
            ControlAction::Disarm {
                arm_jti: "arm-jti-1".into(),
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
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
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(NOW), ConsentChannel::StationAgent));
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
            at(NOW),
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
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
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
            at(NOW),
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
                remote_client_key_id,
                dx_parity,
                ..
            }) => {
                assert_eq!(callsign, "W1XYZ");
                assert!(
                    remote_origin,
                    "remote TxRequest MUST set remote_origin=true"
                );
                assert_eq!(
                    remote_client_key_id.as_deref(),
                    Some(CLIENT_KEY_ID),
                    "remote TxRequest MUST bind the QSO to the requesting peer"
                );
                assert_eq!(dx_parity, Some(pancetta_core::slot::SlotParity::Odd));
            }
            other => panic!("expected StartQso, got {other:?}"),
        }
    }

    // ── Each TxKind maps to the right remote-origin QsoMessage ──────────────
    #[test]
    fn tx_kind_answer_caller_maps_to_remote_respond() {
        let msg = tx_kind_to_qso_message(
            TxKind::AnswerCaller {
                callsign: "K2DEF".into(),
                frequency_hz: 1200.0,
                step: "reportAck".into(),
                dx_parity: Some("even".into()),
                snr: Some(-9.0),
            },
            CLIENT_KEY_ID,
        );
        match msg {
            crate::message_bus::QsoMessage::RespondToCaller {
                callsign,
                step,
                remote_origin,
                remote_client_key_id,
                dx_parity,
                ..
            } => {
                assert_eq!(callsign, "K2DEF");
                assert_eq!(step, pancetta_core::ResponseStep::ReportAck);
                assert_eq!(dx_parity, Some(pancetta_core::slot::SlotParity::Even));
                assert!(remote_origin, "answerCaller MUST be remote_origin=true");
                assert_eq!(remote_client_key_id.as_deref(), Some(CLIENT_KEY_ID));
            }
            other => panic!("expected RespondToCaller, got {other:?}"),
        }
    }

    #[test]
    fn tx_kind_start_cq_maps_to_remote_cq() {
        let msg = tx_kind_to_qso_message(TxKind::StartCq { offset_hz: 800.0 }, CLIENT_KEY_ID);
        match msg {
            crate::message_bus::QsoMessage::StartCq {
                frequency,
                remote_origin,
                remote_client_key_id,
                ..
            } => {
                assert_eq!(frequency, 800);
                assert!(remote_origin, "startCq MUST be remote_origin=true");
                assert_eq!(remote_client_key_id.as_deref(), Some(CLIENT_KEY_ID));
            }
            other => panic!("expected StartCq, got {other:?}"),
        }
    }

    // ========================================================================
    // Task 5: one-controller-at-a-time (free grab, exclusivity, controlState).
    // ========================================================================

    /// A second established peer keyId used by the controller tests (distinct
    /// from `CLIENT_KEY_ID`).
    /// The real keyId of [`peer_b_device`] (seeds 0x33/0x34).
    const PEER_B: &str = "NTL7KxNEiKKhjyoHzW24W0wbgI3bEJcvCtVSaFYXe9Q";

    /// Rule 3: a single connected client that NEVER sends `takeControl` arms
    /// exactly as before — the first control-mutating action (here `Arm`)
    /// implicitly grabs control. This is the zero-behavior-change back-compat
    /// path for today's only real-world case.
    #[tokio::test]
    async fn implicit_grab_on_first_arm_single_client_compat() {
        let mut ctx = ctx_with(true, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        let out = dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert_eq!(out.flow, Dispatch::Continue);
        assert!(
            ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW), ConsentChannel::StationAgent),
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
        ctx.tx_allow_list
            .write()
            .unwrap()
            .insert(PEER_B.to_string());
        ctx.peers.insert(
            PEER_B.to_string(),
            PeerCtx::authorized_until(vec!["qsy".to_string()], NOW + 600_000),
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
            at(NOW),
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
            at(NOW),
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
            .insert(PEER_B.to_string(), PeerCtx::unauthorized());
        // Consent ON: nothing but the sender-binding check gates this arm.
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();

        // `arm_action` names A (CLIENT_KEY_ID) and is signed by A's device key —
        // but the ACTUAL demuxed sender of this frame is B (the `peer` argument).
        let out = dispatch_action(
            arm_action("arm-jti-1"),
            PEER_B,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;

        assert_eq!(out.flow, Dispatch::Continue);
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "a grant naming a DIFFERENT peer than the demuxed sender must never arm"
        );
        assert!(
            !ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW), ConsentChannel::StationAgent),
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
        ctx.tx_allow_list
            .write()
            .unwrap()
            .insert(PEER_B.to_string());
        ctx.peers
            .insert(PEER_B.to_string(), PeerCtx::unauthorized());
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        // A arms (implicit grab → controller=A, armed).
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
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
            at(NOW),
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
            .insert(PEER_B.to_string(), PeerCtx::unauthorized());
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
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
            at(NOW),
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
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
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
            at(NOW),
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
            at(NOW),
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
            with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
            let bus = MessageBus::new(64).unwrap();
            // Pre-arm: implicit grab makes CLIENT_KEY_ID the controller.
            dispatch_action(
                arm_action("arm-jti-1"),
                CLIENT_KEY_ID,
                &mut ctx,
                &bus,
                SESSION_ID,
                at(NOW),
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
            with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
            let bus = MessageBus::new(64).unwrap();
            dispatch_action(
                arm_action("arm-jti-1"),
                CLIENT_KEY_ID,
                &mut ctx,
                &bus,
                SESSION_ID,
                at(NOW),
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
        let device = device_for(connecting_client);
        let mut initiator = initiator_for(&device, &agent_static_pub);
        let msg1 = initiator.write_msg1(&proof_for(&device));

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
    struct TriggerAfterWs<W: WsConn = MockWs> {
        inner: W,
        trigger_after: usize,
        calls: usize,
        on_trigger: Option<Box<dyn FnOnce() + Send>>,
    }

    impl<W: WsConn> TriggerAfterWs<W> {
        fn new(inner: W, trigger_after: usize, on_trigger: impl FnOnce() + Send + 'static) -> Self {
            Self {
                inner,
                trigger_after,
                calls: 0,
                on_trigger: Some(Box::new(on_trigger)),
            }
        }
    }

    impl<W: WsConn> WsConn for TriggerAfterWs<W> {
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
        tx_arm_frame(agent_key_id, session_id, "arm-jti-1", NOW + 300_000)
    }

    /// A signed `txArm` frame with an explicit grant `jti` and `armedUntil`.
    fn tx_arm_frame(agent_key_id: &str, session_id: &str, jti: &str, armed_until: i64) -> Value {
        let mut grant = json!({
            "aud": agent_key_id,
            "clientKeyId": CLIENT_KEY_ID,
            "sessionId": session_id,
            "capabilityJti": "cap-jti-1",
            "operatorCallsign": OPERATOR,
            "armedAt": NOW,
            "armedUntil": armed_until,
            "heartbeatIntervalSec": 10,
            "jti": jti
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
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        let d = dispatch_action(
            action,
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            agent_session_id,
            at(NOW),
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert!(
            ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW), ConsentChannel::StationAgent),
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
            at(NOW),
        )
        .await;
        // jti replay guard is per-ctx (fresh seen set), so this arms the state
        // machine but consent-off denies at the gate.
        assert!(
            !ctx_off
                .arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW), ConsentChannel::StationAgent),
            "consent OFF must deny even after a verified Arm over Noise"
        );
    }

    /// Drive a real Noise handshake (via `run_one_session`'s `MultiPeerSession`,
    /// not `AgentSession` directly) from `connecting_client`, against a station
    /// whose `tx_allow_list` is `allow`. Returns the resulting `ArmContext` so
    /// the caller can inspect `ctx.peers`.
    async fn run_session_with_connecting_peer(
        identity: &AgentIdentity,
        device: &AgentIdentity,
        allow: HashSet<String>,
    ) -> ArmContext {
        let agent_static_pub = identity.agreement_public_raw();
        let mut initiator = initiator_for(device, &agent_static_pub);
        let msg1 = initiator.write_msg1(&proof_for(device));
        let (ctx, _out) = run_session_with_msg1(identity, &device.key_id(), msg1, allow).await;
        ctx
    }

    /// Run one session whose only peer traffic is `msg1` relay-labelled `src`.
    /// Returns the resulting `ArmContext` and every outbound frame.
    async fn run_session_with_msg1(
        identity: &AgentIdentity,
        connecting_client: &str,
        msg1: Vec<u8>,
        allow: HashSet<String>,
    ) -> (ArmContext, Vec<String>) {
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
        let ws = MockWs::new(vec![hello, ready, env_msg1], outbound.clone());

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
            authz_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            authz_generation_seen: 0,
            clock: fixed_clock(NOW),
        };
        let bus = MessageBus::new(64).unwrap();
        run_one_session(ws, identity, &mut ctx, &bus, &mut None).await;
        let out = outbound.lock().unwrap().clone();
        (ctx, out)
    }

    /// Dynamic client selection (Q-0043 quick fix): the peer that actually
    /// connects is learned and vetted — it does NOT need to be the
    /// lexicographically-first entry in `tx_allow_list` (the old, now-removed
    /// pre-pick-at-startup behavior).
    #[tokio::test]
    async fn dynamic_selection_accepts_whichever_allowlisted_peer_connects() {
        let identity = AgentIdentity::generate();
        // PEER_B ("NTL7…") sorts AFTER "000…" — it is not the first entry.
        let connecting_client = PEER_B.to_string();
        let mut allow = HashSet::new();
        allow.insert("000FirstAlphabetically".to_string());
        allow.insert(connecting_client.clone());

        let ctx = run_session_with_connecting_peer(&identity, &peer_b_device(), allow).await;

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
        // A real device with a valid identity proof — but not allow-listed.
        let device = AgentIdentity::generate();
        let connecting_client = device.key_id();
        let mut allow = HashSet::new();
        allow.insert("someOtherAllowlistedClient".to_string());

        let ctx = run_session_with_connecting_peer(&identity, &device, allow).await;

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
            authz_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            authz_generation_seen: 0,
            clock: fixed_clock(NOW),
        }
    }

    /// The test client device: Ed25519 seed 0x22 (== [`client_key`], so every
    /// `clientSig` fixture stays valid) + X25519 secret 0x23.
    fn client_device() -> AgentIdentity {
        AgentIdentity::from_secret_bytes([0x22; 32], [0x23; 32])
    }

    /// A second real device (seeds 0x33/0x34) whose keyId is [`PEER_B`].
    fn peer_b_device() -> AgentIdentity {
        AgentIdentity::from_secret_bytes([0x33; 32], [0x34; 32])
    }

    /// The device whose PROVEN keyId is `kid` (one of the module constants).
    fn device_for(kid: &str) -> AgentIdentity {
        if kid == CLIENT_KEY_ID {
            client_device()
        } else if kid == PEER_B {
            peer_b_device()
        } else {
            panic!("no test device for keyId {kid}")
        }
    }

    /// `device`'s Noise msg1 client identity proof (its enrollment idSig).
    fn proof_for(device: &AgentIdentity) -> Vec<u8> {
        let id_sig = device.sign_domain(
            pancetta_agent::pairing::IDSIG_TAG,
            &device.agreement_public_raw(),
        );
        pancetta_agent::peer_identity::encode_client_identity_proof(
            &device.identity_public_raw(),
            &id_sig,
        )
    }

    /// A client-side Noise IK initiator using `device`'s enrolled agreement
    /// key, against `agent_pub`.
    fn initiator_for(device: &AgentIdentity, agent_pub: &[u8]) -> TestInitiator {
        TestInitiator::new(&device.agreement_private_bytes(), agent_pub)
    }

    /// A capabilityToken for `client_kid` minted on the injected `NOW`
    /// timeline — needed for any hello dispatched inside `run_one_session`,
    /// which reads its wall clock from `ctx.clock` (PAN-97): pinned to `NOW`
    /// in these tests, `ArmTime::now` in production.
    fn injected_clock_token(agent_kid: &str, client_kid: &str, scopes: &[&str]) -> String {
        injected_clock_token_with_jti(
            agent_kid,
            client_kid,
            scopes,
            &format!("hello-{client_kid}"),
        )
    }

    /// [`injected_clock_token`] minted under an explicit `jti` (PAN-96: a freshly
    /// minted token after a revocation carries a new one).
    fn injected_clock_token_with_jti(
        agent_kid: &str,
        client_kid: &str,
        scopes: &[&str],
        jti: &str,
    ) -> String {
        let now_s = NOW / 1000;
        let header = json!({ "alg": "EdDSA", "kid": IDP_KID, "typ": "JWT" });
        let payload = json!({
            "iss": "cqdx", "sub": "acct-1", "operatorCallsign": OPERATOR,
            "aud": agent_kid, "clientKeyId": client_kid,
            "scopes": scopes,
            "iat": now_s - 10, "exp": now_s + 600, "jti": jti
        });
        mint_jws(&header, &payload, &idp_key())
    }

    /// A `qsy`-scoped capabilityToken for CLIENT_KEY_ID with `exp = exp_s`
    /// (epoch seconds), issued at fixed `NOW`.
    fn token_expiring_at(exp_s: i64) -> String {
        let header = json!({ "alg": "EdDSA", "kid": IDP_KID, "typ": "JWT" });
        let payload = json!({
            "iss": "cqdx", "sub": "acct-1", "operatorCallsign": OPERATOR,
            "aud": AGENT_KEY_ID, "clientKeyId": CLIENT_KEY_ID,
            "scopes": ["status", "qsy"],
            "iat": NOW / 1000 - 10, "exp": exp_s, "jti": "short-lived"
        });
        mint_jws(&header, &payload, &idp_key())
    }

    /// The rig-api.v1 `hello` client frame carrying `token`.
    fn hello_frame_bytes(token: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "frame": "hello",
            "hello": { "protocolVersion": 1, "capabilityToken": token }
        }))
        .unwrap()
    }

    /// Client-side transport encrypt.
    fn client_encrypt(t: &mut snow::TransportState, plaintext: &[u8]) -> Vec<u8> {
        let mut ct = vec![0u8; plaintext.len() + 16];
        let n = t.write_message(plaintext, &mut ct).unwrap();
        ct.truncate(n);
        ct
    }

    /// Client-side transport decrypt into JSON.
    fn client_decrypt_json(t: &mut snow::TransportState, ct: &[u8]) -> Value {
        let mut buf = vec![0u8; ct.len().max(1)];
        let n = t.read_message(ct, &mut buf).unwrap();
        buf.truncate(n);
        serde_json::from_slice(&buf).unwrap()
    }

    /// Every env payload addressed to `peer` in `out`, in wire order.
    fn envs_to(out: &[String], peer: &str) -> Vec<Vec<u8>> {
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
    }

    /// Pins the test constants to real, self-certifying keyIds.
    #[test]
    fn test_device_constants_match_their_keys() {
        assert_eq!(client_device().key_id(), CLIENT_KEY_ID);
        assert_eq!(peer_b_device().key_id(), PEER_B);
        assert_eq!(
            client_device().identity_public_raw(),
            client_key().verifying_key().to_bytes(),
            "client_device's identity key is client_key — clientSig fixtures stay valid"
        );
    }

    /// Gherkin Scenario 1 at the `run_one_session` level: the relay labels the
    /// env with an admitted client's id and replays that client's genuine
    /// proof, but runs the handshake with ITS OWN static key.
    #[tokio::test]
    async fn run_one_session_refuses_relay_keyed_impersonation() {
        let identity = AgentIdentity::generate();
        let mut allow = HashSet::new();
        allow.insert(CLIENT_KEY_ID.to_string());
        let relay = AgentIdentity::generate();
        let mut relay_init = initiator_for(&relay, &identity.agreement_public_raw());
        let msg1 = relay_init.write_msg1(&proof_for(&client_device()));

        let (ctx, out) = run_session_with_msg1(&identity, CLIENT_KEY_ID, msg1, allow).await;

        assert!(
            !ctx.peers.contains_key(CLIENT_KEY_ID),
            "a relay-keyed session must never be treated as the claimed client"
        );
        assert!(ctx.peers.is_empty());
        assert!(
            envs_to(&out, CLIENT_KEY_ID).is_empty(),
            "no msg2 or any other frame may be sent to the impersonated label"
        );
    }

    /// A hand-driven `MultiPeerSession` over the clonable `MockWs` with each of
    /// `devices` established (real handshakes with identity proofs) and its
    /// client transport completed. `ctx.peers` is left for the caller to set.
    fn hand_session<'a>(
        identity: &'a AgentIdentity,
        devices: &[&AgentIdentity],
    ) -> (
        MultiPeerSession<'a, MockWs>,
        Arc<Mutex<Vec<String>>>,
        Vec<snow::TransportState>,
    ) {
        let (sess, outbound, transports, _ws) = hand_session_ws(identity, devices);
        (sess, outbound, transports)
    }

    /// [`hand_session`], also returning a handle to the session's `MockWs` so
    /// a test can push further inbound frames.
    #[allow(clippy::type_complexity)]
    fn hand_session_ws<'a>(
        identity: &'a AgentIdentity,
        devices: &[&AgentIdentity],
    ) -> (
        MultiPeerSession<'a, MockWs>,
        Arc<Mutex<Vec<String>>>,
        Vec<snow::TransportState>,
        MockWs,
    ) {
        let outbound = Arc::new(Mutex::new(Vec::new()));
        let ws = MockWs::new(Vec::new(), outbound.clone());
        let handle = ws.clone();
        let allow: HashSet<String> = devices.iter().map(|d| d.key_id()).collect();
        let mut sess = MultiPeerSession::new(ws, identity, allow);
        let mut transports = Vec::new();
        for device in devices {
            let kid = device.key_id();
            let mut init = initiator_for(device, &identity.agreement_public_raw());
            let msg1 = init.write_msg1(&proof_for(device));
            handle.push_inbound(
                RelayFrame::Env {
                    dst: identity.key_id(),
                    payload: b64url(&msg1),
                    src: Some(kid.clone()),
                }
                .to_json()
                .unwrap(),
            );
            match sess
                .process_next(std::time::Duration::from_millis(1))
                .unwrap()
            {
                Poll::PeerEstablished { peer, .. } => assert_eq!(peer, kid),
                other => panic!("expected PeerEstablished, got {other:?}"),
            }
            let msg2 = envs_to(&outbound.lock().unwrap(), &kid)
                .pop()
                .expect("msg2 on the wire");
            init.read_msg2(&msg2);
            transports.push(init.into_transport());
        }
        (sess, outbound, transports, handle)
    }

    /// A display-feed receiver with `ev` already buffered. The sender is
    /// dropped; a tokio broadcast receiver still yields buffered values first.
    fn feed_with(
        ev: pancetta_protocol::ServerEvent,
    ) -> Option<tokio::sync::broadcast::Receiver<pancetta_protocol::ServerEvent>> {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        tx.send(ev).unwrap();
        Some(rx)
    }

    #[tokio::test]
    async fn pan96_hello_renewal_after_expiry_disarms_before_replacement() {
        let identity = AgentIdentity::generate();
        let agent_kid = identity.key_id();
        let mut ctx = fresh_ctx(&agent_kid, HashSet::from([CLIENT_KEY_ID.to_string()]));
        // The session reads arm time from `ctx.clock`, which `fresh_ctx` pins
        // to this module's `NOW` (PAN-97), so the grant is armed on that same
        // injected timeline and the expiry crossing below advances a manual
        // clock instead of sleeping.
        let t = Arc::new(std::sync::atomic::AtomicI64::new(NOW));
        ctx.clock = manual_clock(&t);
        ctx.arm
            .lock()
            .unwrap()
            .set_channel_consent(ConsentChannel::StationAgent, true, NOW);
        ctx.arm.lock().unwrap().arm(
            pancetta_agent::arm::VerifiedArmGrant {
                operator_callsign: OPERATOR.to_string(),
                ttl_ms: 3_600_000,
                armed_until_ms: NOW + 3_600_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: "renewal-arm".to_string(),
                client_key_id: CLIENT_KEY_ID.to_string(),
            },
            at(NOW),
        );
        ctx.controller = Some(CLIENT_KEY_ID.to_string());
        let device = client_device();
        let mut initiator = initiator_for(&device, &identity.agreement_public_raw());
        let msg1 = initiator.write_msg1(&proof_for(&device));
        let outbound = Arc::new(Mutex::new(Vec::new()));
        let mock = MockWs::new(
            vec![
                RelayFrame::Hello {
                    challenge: b64url(&[7; 32]),
                }
                .to_json()
                .unwrap(),
                RelayFrame::Env {
                    dst: agent_kid.clone(),
                    payload: b64url(&msg1),
                    src: Some(CLIENT_KEY_ID.to_string()),
                }
                .to_json()
                .unwrap(),
            ],
            outbound.clone(),
        );
        let handle = mock.clone();
        let expires = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let expiry = expires.clone();
        let hello_outbound = outbound.clone();
        let hello_ws = TriggerAfterWs::new(mock, 3, move || {
            let msg2 = envs_to(&hello_outbound.lock().unwrap(), CLIENT_KEY_ID).remove(0);
            initiator.read_msg2(&msg2);
            let mut transport = initiator.into_transport();
            let exp_ms = NOW + 2_000;
            expiry.store(exp_ms, Ordering::Release);
            let short_token = mint_jws(
                &json!({"alg": "EdDSA", "kid": IDP_KID, "typ": "JWT"}),
                &json!({
                    "iss": "cqdx", "sub": "acct-1", "operatorCallsign": OPERATOR,
                    "aud": agent_kid, "clientKeyId": CLIENT_KEY_ID,
                    "scopes": ["status"], "iat": exp_ms / 1000 - 10, "exp": exp_ms / 1000,
                    "jti": "renewal-old",
                }),
                &idp_key(),
            );
            let fresh_token = injected_clock_token_with_jti(
                &agent_kid,
                CLIENT_KEY_ID,
                &["status"],
                "renewal-new",
            );
            for token in [short_token, fresh_token] {
                let ct = client_encrypt(&mut transport, &hello_frame_bytes(&token));
                handle.push_inbound(
                    RelayFrame::Env {
                        dst: agent_kid.clone(),
                        payload: b64url(&ct),
                        src: Some(CLIENT_KEY_ID.to_string()),
                    }
                    .to_json()
                    .unwrap(),
                );
            }
        });
        let arm = ctx.arm.clone();
        let clock = t.clone();
        let ws = TriggerAfterWs::new(hello_ws, 4, move || {
            assert!(
                arm.lock().unwrap().tx_permitted(
                    at(clock.load(Ordering::Acquire)),
                    ConsentChannel::StationAgent
                ),
                "old hello retained the live arm"
            );
            // Advance the injected clock past the short token's expiry
            // instead of sleeping.
            clock.store(expires.load(Ordering::Acquire) + 1_000, Ordering::Release);
        });
        let bus = MessageBus::new(64).unwrap();
        run_one_session(ws, &identity, &mut ctx, &bus, &mut None).await;
        assert_eq!(
            ctx.peers[CLIENT_KEY_ID].auth.as_ref().unwrap().jti,
            "renewal-new"
        );
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "renewal must not preserve an arm across expiry"
        );
        assert_eq!(
            ctx.controller, None,
            "expiry must release the controller before renewal"
        );
    }

    #[tokio::test]
    async fn pan96_outbound_rechecks_time_for_each_frame() {
        // Exercise both outbound paths. The first check is before expiry;
        // every later check is at expiry, without wall-clock scheduling races.
        let mut sent = Vec::new();
        for (display_feed, two_peers) in
            [(true, false), (false, false), (true, true), (false, true)]
        {
            let identity = AgentIdentity::generate();
            let devices = [client_device(), peer_b_device()];
            let peers: Vec<_> = devices.iter().take(if two_peers { 2 } else { 1 }).collect();
            let allow = peers.iter().map(|p| p.key_id()).collect();
            let mut ctx = fresh_ctx(&identity.key_id(), allow);
            let (mut sess, outbound, _transports) = hand_session(&identity, &peers);
            for peer in &peers {
                ctx.peers.insert(
                    peer.key_id(),
                    PeerCtx::authorized_until(vec!["status".to_string()], NOW + 1),
                );
            }
            let time = std::cell::Cell::new(NOW);
            let clock = || {
                let now = time.get();
                time.set(NOW + 1);
                now
            };
            let before = outbound.lock().unwrap().len();
            if display_feed {
                let (tx, rx) = tokio::sync::broadcast::channel(16);
                tx.send(pancetta_protocol::ServerEvent::TxStatus { active: true })
                    .unwrap();
                if !two_peers {
                    tx.send(pancetta_protocol::ServerEvent::TxStatus { active: false })
                        .unwrap();
                }
                drain_read_stream(&mut Some(rx), &mut sess, &ctx, clock);
            } else {
                let mut sends = control_state_sends(&ctx, at(NOW), None);
                if !two_peers {
                    sends.extend(control_state_sends(&ctx, at(NOW), None));
                }
                deliver_sends(&mut sess, &ctx, &sends, clock);
            }
            sent.push(outbound.lock().unwrap().len() - before);
        }
        assert_eq!(
            sent,
            vec![1, 1, 1, 1],
            "each outbound path must withhold frames at expiry"
        );

        // A full display backlog must yield to housekeeping with work left.
        let identity = AgentIdentity::generate();
        let ctx = fresh_ctx(&identity.key_id(), HashSet::new());
        let (mut sess, _outbound, _transports) = hand_session(&identity, &[]);
        let (tx, rx) = tokio::sync::broadcast::channel(1024);
        for _ in 0..1024 {
            tx.send(pancetta_protocol::ServerEvent::TxStatus { active: true })
                .unwrap();
        }
        let mut events = Some(rx);
        drain_read_stream(&mut events, &mut sess, &ctx, || NOW);
        assert_eq!(events.as_ref().unwrap().len(), 1024 - 64);
    }

    /// Gherkin Scenario 2: a peer that established Noise but holds no verified
    /// hello capability receives no read-stream event; a hello-authorized
    /// peer does. (Replaces the pre-PAN-95 test that asserted every
    /// established transport received the event.)
    #[tokio::test]
    async fn read_stream_events_reach_only_hello_authorized_peers() {
        let identity = AgentIdentity::generate();
        let mut allow = HashSet::new();
        allow.insert(CLIENT_KEY_ID.to_string());
        allow.insert(PEER_B.to_string());
        let mut ctx = fresh_ctx(&identity.key_id(), allow);
        let (dev_a, dev_b) = (client_device(), peer_b_device());
        let (mut sess, outbound, mut transports) = hand_session(&identity, &[&dev_a, &dev_b]);
        ctx.peers.insert(
            CLIENT_KEY_ID.to_string(),
            PeerCtx::authorized_until(vec!["status".to_string()], NOW + 60_000),
        );
        ctx.peers
            .insert(PEER_B.to_string(), PeerCtx::unauthorized());

        let mut events = feed_with(pancetta_protocol::ServerEvent::TxStatus { active: true });
        let before = outbound.lock().unwrap().len();
        drain_read_stream(&mut events, &mut sess, &ctx, || NOW);

        let out = outbound.lock().unwrap().clone();
        let new = &out[before..];
        assert_eq!(
            new.len(),
            1,
            "exactly one env — for the authorized peer: {new:?}"
        );
        let envs_a = envs_to(new, CLIENT_KEY_ID);
        assert_eq!(envs_a.len(), 1, "the one env is addressed to A");
        let v = client_decrypt_json(&mut transports[0], &envs_a[0]);
        assert_eq!(v["frame"], "event");
        assert_eq!(v["event"]["event"], "txStatus");
        assert_eq!(v["event"]["active"], true);
        assert_eq!(
            envs_to(&out, PEER_B).len(),
            1,
            "B (no hello) has only its msg2 on the wire — no event"
        );
    }

    #[tokio::test]
    async fn read_stream_withheld_after_capability_expiry() {
        let identity = AgentIdentity::generate();
        let mut allow = HashSet::new();
        allow.insert(CLIENT_KEY_ID.to_string());
        let mut ctx = fresh_ctx(&identity.key_id(), allow);
        let dev_a = client_device();
        let (mut sess, outbound, _t) = hand_session(&identity, &[&dev_a]);

        // Expired one ms ago, and expiring exactly now: both withheld.
        for exp in [NOW - 1, NOW] {
            ctx.peers.insert(
                CLIENT_KEY_ID.to_string(),
                PeerCtx::authorized_until(vec!["status".to_string(), "qsy".to_string()], exp),
            );
            let mut events = feed_with(pancetta_protocol::ServerEvent::TxStatus { active: true });
            let before = outbound.lock().unwrap().len();
            drain_read_stream(&mut events, &mut sess, &ctx, || NOW);
            assert_eq!(
                outbound.lock().unwrap().len(),
                before,
                "a capability expired at exp={exp} (now={NOW}) must receive nothing"
            );
        }
    }

    #[tokio::test]
    async fn status_scope_suffices_for_read_stream() {
        let identity = AgentIdentity::generate();
        let mut allow = HashSet::new();
        allow.insert(CLIENT_KEY_ID.to_string());
        allow.insert(PEER_B.to_string());
        let mut ctx = fresh_ctx(&identity.key_id(), allow);
        let (dev_a, dev_b) = (client_device(), peer_b_device());
        let (mut sess, outbound, mut transports) = hand_session(&identity, &[&dev_a, &dev_b]);
        // A holds only the lowest tier; B holds a verified token whose scopes
        // name no rig-api tier at all.
        ctx.peers.insert(
            CLIENT_KEY_ID.to_string(),
            PeerCtx::authorized_until(vec!["status".to_string()], NOW + 60_000),
        );
        ctx.peers.insert(
            PEER_B.to_string(),
            PeerCtx::authorized_until(vec!["bogus".to_string()], NOW + 60_000),
        );

        let mut events = feed_with(pancetta_protocol::ServerEvent::TxStatus { active: false });
        let before = outbound.lock().unwrap().len();
        drain_read_stream(&mut events, &mut sess, &ctx, || NOW);
        let out = outbound.lock().unwrap().clone();
        let envs_a = envs_to(&out[before..], CLIENT_KEY_ID);
        assert_eq!(envs_a.len(), 1, "status scope suffices for the read stream");
        let v = client_decrypt_json(&mut transports[0], &envs_a[0]);
        assert_eq!(v["event"]["event"], "txStatus");
        assert!(
            envs_to(&out[before..], PEER_B).is_empty(),
            "a scope below `status` grants no read access"
        );
    }

    #[tokio::test]
    async fn deliver_sends_drops_frames_for_unauthorized_peers() {
        let identity = AgentIdentity::generate();
        let mut allow = HashSet::new();
        allow.insert(CLIENT_KEY_ID.to_string());
        allow.insert(PEER_B.to_string());
        let mut ctx = fresh_ctx(&identity.key_id(), allow);
        let (dev_a, dev_b) = (client_device(), peer_b_device());
        let (mut sess, outbound, mut transports) = hand_session(&identity, &[&dev_a, &dev_b]);
        ctx.peers.insert(
            CLIENT_KEY_ID.to_string(),
            PeerCtx::authorized_until(vec!["status".to_string()], NOW + 60_000),
        );
        ctx.peers
            .insert(PEER_B.to_string(), PeerCtx::unauthorized());

        // One controlState frame per established peer, as every transition
        // builds them — plus an error frame addressed to B.
        let mut sends = control_state_sends(&ctx, at(NOW), None);
        assert_eq!(sends.len(), 2, "frames are still BUILT for every peer");
        sends.push(PeerSend {
            to: SendTarget::One(PEER_B.to_string()),
            frame: pancetta_protocol::ServerFrame::Event {
                event: pancetta_protocol::ServerEvent::Error {
                    component: "stationAgent".into(),
                    message: "another client holds control — takeControl first".into(),
                },
            },
        });
        let before = outbound.lock().unwrap().len();
        deliver_sends(&mut sess, &ctx, &sends, || NOW);

        let out = outbound.lock().unwrap().clone();
        let new = &out[before..];
        assert_eq!(
            new.len(),
            1,
            "only the authorized peer's frame is delivered: {new:?}"
        );
        let envs_a = envs_to(new, CLIENT_KEY_ID);
        assert_eq!(envs_a.len(), 1);
        let v = client_decrypt_json(&mut transports[0], &envs_a[0]);
        assert_eq!(v["event"]["event"], "controlState");
        assert!(
            envs_to(new, PEER_B).is_empty(),
            "nothing, not even an error, to B"
        );
    }

    #[tokio::test]
    async fn valid_hello_returns_control_state_greeting_for_that_peer_only() {
        let mut ctx = ctx_with(true, true);
        ctx.peers
            .insert(PEER_B.to_string(), PeerCtx::unauthorized());
        let bus = MessageBus::new(64).unwrap();
        let d = dispatch_action(
            ControlAction::Hello {
                capability_token: Some(valid_token()),
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert_eq!(d.flow, Dispatch::Continue);
        assert_eq!(d.sends.len(), 1, "one greeting: {:?}", d.sends);
        let SendTarget::One(to) = &d.sends[0].to;
        assert_eq!(to, CLIENT_KEY_ID, "greeting goes to the hello'd peer only");
        assert!(matches!(
            d.sends[0].frame,
            pancetta_protocol::ServerFrame::Event {
                event: pancetta_protocol::ServerEvent::ControlState { .. }
            }
        ));
        let auth = ctx.peers[CLIENT_KEY_ID].auth.as_ref().expect("auth stored");
        assert_eq!(auth.exp_ms, (NOW / 1000 + 600) * 1000, "token exp is kept");
        assert!(ctx.peers[PEER_B].auth.is_none(), "B is untouched");
    }

    #[tokio::test]
    async fn invalid_hello_returns_no_frames_and_clears_auth() {
        let bus = MessageBus::new(64).unwrap();
        for token in [Some("not.a.jws".to_string()), None] {
            let mut ctx = ctx_with(true, true);
            ctx.peers.insert(
                CLIENT_KEY_ID.to_string(),
                PeerCtx::authorized_until(vec!["status".to_string()], NOW + 60_000),
            );
            let d = dispatch_action(
                ControlAction::Hello {
                    capability_token: token.clone(),
                },
                CLIENT_KEY_ID,
                &mut ctx,
                &bus,
                SESSION_ID,
                at(NOW),
            )
            .await;
            assert!(d.sends.is_empty(), "token {token:?}: no frames");
            assert!(
                ctx.peers[CLIENT_KEY_ID].auth.is_none(),
                "token {token:?}: a failed hello clears any prior capability"
            );
            assert!(!peer_may_receive(&ctx, CLIENT_KEY_ID, NOW));
        }
    }

    #[tokio::test]
    async fn qsy_refused_after_hello_token_expires() {
        let mut ctx = ctx_with(true, true);
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let hello = ControlAction::Hello {
            capability_token: Some(token_expiring_at(NOW / 1000 + 5)),
        };
        dispatch_action(hello, CLIENT_KEY_ID, &mut ctx, &bus, SESSION_ID, at(NOW)).await;
        let qsy = || ControlAction::Qsy {
            vfo: 0,
            frequency_hz: 14_074_000.0,
        };
        // Sanity: within the token's lifetime the qsy is forwarded.
        dispatch_action(qsy(), CLIENT_KEY_ID, &mut ctx, &bus, SESSION_ID, at(NOW)).await;
        assert!(hamlib_rx.try_recv().is_ok(), "qsy permitted before exp");
        // A minute later the capability has expired.
        dispatch_action(
            qsy(),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW + 60_000),
        )
        .await;
        assert!(
            hamlib_rx.try_recv().is_err(),
            "an expired hello capability must not drive setFrequency"
        );
    }

    #[tokio::test]
    async fn set_split_refused_after_hello_token_expires() {
        let mut ctx = ctx_with(true, true);
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let hello = ControlAction::Hello {
            capability_token: Some(token_expiring_at(NOW / 1000 + 5)),
        };
        dispatch_action(hello, CLIENT_KEY_ID, &mut ctx, &bus, SESSION_ID, at(NOW)).await;
        let split = || ControlAction::SetSplit {
            enabled: true,
            tx_frequency_hz: 14_075_000.0,
        };
        dispatch_action(split(), CLIENT_KEY_ID, &mut ctx, &bus, SESSION_ID, at(NOW)).await;
        assert!(
            hamlib_rx.try_recv().is_ok(),
            "setSplit permitted before exp"
        );
        dispatch_action(
            split(),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW + 60_000),
        )
        .await;
        assert!(
            hamlib_rx.try_recv().is_err(),
            "an expired hello capability must not drive setSplit"
        );
    }

    #[test]
    fn expired_peer_auth_is_swept_once() {
        let mut ctx = ctx_with(true, true);
        ctx.peers.insert(
            CLIENT_KEY_ID.to_string(),
            PeerCtx::authorized_until(vec!["status".to_string()], NOW + 1_000),
        );
        ctx.peers.insert(
            PEER_B.to_string(),
            PeerCtx::authorized_until(vec!["status".to_string()], NOW + 600_000),
        );
        assert!(
            sweep_expired_peer_auth(&mut ctx, NOW).is_empty(),
            "nothing expired yet"
        );
        assert_eq!(
            sweep_expired_peer_auth(&mut ctx, NOW + 1_000),
            vec![CLIENT_KEY_ID.to_string()]
        );
        assert!(
            ctx.peers[CLIENT_KEY_ID].auth.is_none(),
            "expired auth cleared"
        );
        assert!(ctx.peers[PEER_B].auth.is_some(), "unexpired auth kept");
        assert!(
            sweep_expired_peer_auth(&mut ctx, NOW + 2_000).is_empty(),
            "an already-swept peer is not reported again"
        );
    }

    /// A controller whose hello capability expires is sent nothing from then
    /// on, so it must not stay armed: expiry disarms it and releases control,
    /// and the still-authorized peers are told.
    #[tokio::test]
    async fn controller_capability_expiry_disarms_and_releases_control() {
        let mut ctx = ctx_with(true, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            arm_action("expiry-arm-jti"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(ctx.arm.lock().unwrap().is_armed(), "setup: armed");
        assert_eq!(ctx.controller.as_deref(), Some(CLIENT_KEY_ID));
        ctx.peers.insert(
            CLIENT_KEY_ID.to_string(),
            PeerCtx::authorized_until(vec!["status".to_string()], NOW + 1_000),
        );
        ctx.peers.insert(
            PEER_B.to_string(),
            PeerCtx::authorized_until(vec!["status".to_string()], NOW + 600_000),
        );

        assert!(
            expire_peer_auth(&mut ctx, at(NOW)).is_empty(),
            "nothing expired yet"
        );
        assert!(ctx.arm.lock().unwrap().is_armed());

        let sends = expire_peer_auth(&mut ctx, at(NOW + 1_000));
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "an expired controller capability must disarm"
        );
        assert_eq!(ctx.controller, None, "and release control");
        assert!(
            sends
                .iter()
                .any(|s| matches!(&s.to, SendTarget::One(p) if p == PEER_B)),
            "the remaining peers get the updated controlState"
        );

        // A non-controller expiring disarms nothing.
        let mut ctx = ctx_with(true, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        dispatch_action(
            arm_action("expiry-arm-jti-2"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        ctx.peers.insert(
            PEER_B.to_string(),
            PeerCtx::authorized_until(vec!["status".to_string()], NOW + 1_000),
        );
        assert!(expire_peer_auth(&mut ctx, at(NOW + 1_000)).is_empty());
        assert!(
            ctx.arm.lock().unwrap().is_armed(),
            "a listener expiring disarms nothing"
        );
        assert_eq!(ctx.controller.as_deref(), Some(CLIENT_KEY_ID));
    }

    /// Gherkin Scenario 2 end-to-end through `run_one_session`: two devices
    /// establish real Noise sessions; only the one that sends a valid hello
    /// receives anything, and its controlState greeting comes AFTER its hello.
    #[tokio::test]
    async fn no_frame_reaches_a_peer_until_it_sends_a_valid_hello() {
        let identity = AgentIdentity::generate();
        let agent_kid = identity.key_id();
        let agent_pub = identity.agreement_public_raw();
        let mut allow = HashSet::new();
        allow.insert(CLIENT_KEY_ID.to_string());
        allow.insert(PEER_B.to_string());
        let mut ctx = fresh_ctx(&agent_kid, allow);
        let bus = MessageBus::new(64).unwrap();

        let (dev_a, dev_b) = (client_device(), peer_b_device());
        let mut init_a = initiator_for(&dev_a, &agent_pub);
        let mut init_b = initiator_for(&dev_b, &agent_pub);
        let msg1_a = init_a.write_msg1(&proof_for(&dev_a));
        let msg1_b = init_b.write_msg1(&proof_for(&dev_b));
        let env = |payload: &[u8], src: &str| {
            RelayFrame::Env {
                dst: agent_kid.clone(),
                payload: b64url(payload),
                src: Some(src.to_string()),
            }
            .to_json()
            .unwrap()
        };
        let hello = RelayFrame::Hello {
            challenge: b64url(&[21u8; 32]),
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
        // recv #1 hello, #2 env A, #3 env B. The trigger fires just before
        // recv #4: it queues A's encrypted hello plus an idle tick and
        // publishes one display event. The event is drained right after the
        // hello's dispatch on #4 (and again, idle, on #5) — B, established
        // but hello-less the whole time, must get none of it.
        let mock = MockWs::new(
            vec![hello, env(&msg1_a, CLIENT_KEY_ID), env(&msg1_b, PEER_B)],
            outbound.clone(),
        );
        let ws_handle = mock.clone();
        let trigger_outbound = outbound.clone();
        let trigger_agent_kid = agent_kid.clone();
        let (evt_tx, evt_rx) = tokio::sync::broadcast::channel(16);
        let mut events = Some(evt_rx);
        let trigger_evt_tx = evt_tx.clone();
        let transport_slot: Arc<Mutex<Option<snow::TransportState>>> = Arc::new(Mutex::new(None));
        let slot = transport_slot.clone();
        let ws = TriggerAfterWs::new(mock, 4, move || {
            // Both peers are established and nothing but msg2 has been sent.
            let out = trigger_outbound.lock().unwrap().clone();
            assert_eq!(envs_to(&out, CLIENT_KEY_ID).len(), 1, "A: only msg2 so far");
            assert_eq!(envs_to(&out, PEER_B).len(), 1, "B: only msg2 so far");
            let msg2_a = envs_to(&out, CLIENT_KEY_ID).remove(0);
            init_a.read_msg2(&msg2_a);
            let mut t = init_a.into_transport();
            let token = injected_clock_token(&trigger_agent_kid, CLIENT_KEY_ID, &["status"]);
            let ct = client_encrypt(&mut t, &hello_frame_bytes(&token));
            *slot.lock().unwrap() = Some(t);
            ws_handle.push_inbound(
                RelayFrame::Env {
                    dst: trigger_agent_kid.clone(),
                    payload: b64url(&ct),
                    src: Some(CLIENT_KEY_ID.to_string()),
                }
                .to_json()
                .unwrap(),
            );
            ws_handle.push_inbound(idle_tick);
            trigger_evt_tx
                .send(pancetta_protocol::ServerEvent::TxStatus { active: true })
                .unwrap();
        });

        run_one_session(ws, &identity, &mut ctx, &bus, &mut events).await;
        drop(evt_tx);

        assert!(ctx.peers.contains_key(CLIENT_KEY_ID));
        assert!(ctx.peers.contains_key(PEER_B));
        let out = outbound.lock().unwrap().clone();

        let envs_b = envs_to(&out, PEER_B);
        assert_eq!(
            envs_b.len(),
            1,
            "the peer that never sent hello gets its msg2 and NOTHING else"
        );

        let envs_a = envs_to(&out, CLIENT_KEY_ID);
        assert_eq!(envs_a.len(), 3, "A: msg2, the hello greeting, the event");
        let mut t = transport_slot.lock().unwrap().take().unwrap();
        let greeting = client_decrypt_json(&mut t, &envs_a[1]);
        assert_eq!(greeting["frame"], "event");
        assert_eq!(greeting["event"]["event"], "controlState");
        let event = client_decrypt_json(&mut t, &envs_a[2]);
        assert_eq!(event["event"]["event"], "txStatus");
        assert_eq!(event["event"]["active"], true);
    }

    /// Task 6: with no display feed subscribed (`events: None` — the inert
    /// path when neither the localhost gateway nor `station_agent_active`
    /// started one), `drain_read_stream` must be a strict no-op: a
    /// `Poll::Idle` tick after a peer establishes sends nothing beyond the
    /// ordinary handshake traffic. Since PAN-95 establishment itself sends no
    /// greeting — the peer has not sent a `hello` yet, so it gets nothing.
    #[tokio::test]
    async fn read_stream_absent_feed_is_inert() {
        let identity = AgentIdentity::generate();
        let agent_kid = identity.key_id();
        let agent_pub = identity.agreement_public_raw();

        let mut allow = HashSet::new();
        allow.insert(CLIENT_KEY_ID.to_string());
        let mut ctx = fresh_ctx(&agent_kid, allow);
        let bus = MessageBus::new(64).unwrap();

        let device = client_device();
        let mut init = initiator_for(&device, &agent_pub);
        let msg1 = init.write_msg1(&proof_for(&device));

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

        // auth (relay leg) + msg2 (handshake) — and NOTHING else: no greeting
        // (no hello yet), and the trailing Idle tick (from `presence:up`) must
        // not have added a single outbound frame.
        let out = outbound.lock().unwrap().clone();
        assert_eq!(
            out.len(),
            2,
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
            authz_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            authz_generation_seen: 0,
            clock: fixed_clock(NOW),
        }
    }

    #[test]
    fn authorizations_filter_matches_only_this_agents_edges() {
        let edges = [
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
        let edges = [
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
        let authz_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
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
            authz_generation.clone(),
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
        assert_eq!(
            authz_generation.load(Ordering::Acquire),
            0,
            "a failing poll must never bump the authorization generation (PAN-96)"
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
    /// while holding `ctx.arm`'s lock, immediately before `st.arm(at(...))`)
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
        // as verify_and_arm does right before st.arm(at(...)) — must now see the
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
            st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
            st.arm(
                pancetta_agent::arm::VerifiedArmGrant {
                    operator_callsign: "K5ARH".to_string(),
                    ttl_ms: 3_600_000, // 1h — long enough that only the poll-driven disarm can end it
                    armed_until_ms: NOW + 3_600_000,
                    heartbeat_interval_sec: 10,
                    scope_tx: true,
                    jti: "revocation-test-jti".to_string(),
                    client_key_id: CLIENT_KEY_ID.to_string(),
                },
                at(NOW),
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
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
        // session starts — so the peer's hello greeting shows
        // `transmitArmed: true`, giving the later broadcast a real
        // true→false transition to prove. `run_one_session` reads arm time
        // from `ctx.clock`, which `fresh_ctx` pins to this module's `NOW`, so
        // the grant is armed on that same injected timeline.
        let real_now = NOW;
        ctx.arm
            .lock()
            .unwrap()
            .set_channel_consent(ConsentChannel::StationAgent, true, real_now);
        ctx.arm.lock().unwrap().arm(
            pancetta_agent::arm::VerifiedArmGrant {
                operator_callsign: OPERATOR.to_string(),
                ttl_ms: 3_600_000,
                armed_until_ms: real_now + 3_600_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: "broadcast-test-jti".to_string(),
                client_key_id: CLIENT_KEY_ID.to_string(),
            },
            at(real_now),
        );
        assert!(
            ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(real_now), ConsentChannel::StationAgent),
            "setup sanity: must be armed+permitted before the peer establishes"
        );

        // Clones of the shared state a concurrent poll task would hold —
        // moved into the trigger closure below to simulate its disarm.
        let poll_arm = ctx.arm.clone();
        let poll_revocation_pending = ctx.revocation_pending.clone();

        let bus = MessageBus::new(64).unwrap();
        let device = client_device();
        let mut initiator = initiator_for(&device, &agent_pub);
        let msg1 = initiator.write_msg1(&proof_for(&device));

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
        let mock = MockWs::new(vec![hello, env_msg1], outbound.clone());
        // PAN-95: the peer receives nothing until it sends a valid hello, so
        // right before recv #3 (after CLIENT_KEY_ID established) complete its
        // client transport and queue an encrypted real-clock hello, then the
        // idle tick. The hello yields the controlState greeting (armed=true).
        let ws_handle = mock.clone();
        let hello_outbound = outbound.clone();
        let hello_agent_kid = agent_kid.clone();
        let transport_slot: Arc<Mutex<Option<snow::TransportState>>> = Arc::new(Mutex::new(None));
        let slot = transport_slot.clone();
        let hello_ws = TriggerAfterWs::new(mock, 3, move || {
            let msg2 = envs_to(&hello_outbound.lock().unwrap(), CLIENT_KEY_ID).remove(0);
            initiator.read_msg2(&msg2);
            let mut t = initiator.into_transport();
            let token = injected_clock_token(&hello_agent_kid, CLIENT_KEY_ID, &["status"]);
            let ct = client_encrypt(&mut t, &hello_frame_bytes(&token));
            *slot.lock().unwrap() = Some(t);
            ws_handle.push_inbound(
                RelayFrame::Env {
                    dst: hello_agent_kid.clone(),
                    payload: b64url(&ct),
                    src: Some(CLIENT_KEY_ID.to_string()),
                }
                .to_json()
                .unwrap(),
            );
            ws_handle.push_inbound(idle_tick);
        });
        // Fires right before recv #4 (idle_tick) is served — i.e. AFTER
        // CLIENT_KEY_ID has established AND been greeted on its hello.
        let ws = TriggerAfterWs::new(hello_ws, 4, move || {
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
        // hello greeting (armed=true), then the poll-driven broadcast
        // (armed=false).
        let out = outbound.lock().unwrap().clone();
        let envs = envs_to(&out, CLIENT_KEY_ID);
        assert_eq!(
            envs.len(),
            3,
            "msg2 + the hello greeting + the poll-driven broadcast"
        );

        let mut transport = transport_slot.lock().unwrap().take().unwrap();
        let mut decrypt = |ct: &[u8]| client_decrypt_json(&mut transport, ct);

        let greeting = decrypt(&envs[1]);
        assert_eq!(greeting["event"]["event"], "controlState");
        assert_eq!(
            greeting["event"]["transmitArmed"], true,
            "the hello greeting must reflect the pre-armed state"
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

    /// PAN-34: a persistently misbehaving established client (one that keeps
    /// sending frames that decrypt fine but fail `map_client_frame`) must not
    /// be able to suppress the poll-driven revocation broadcast to OTHER,
    /// well-behaved connected peers. Before the fix, the malformed-frame arm
    /// of `run_one_session`'s match `continue`d straight back to the top of
    /// the loop, skipping the trailing `revocation_pending` check entirely —
    /// so PEER_B here would still see its stale pre-revocation `controlState`
    /// with no way to learn the station was disarmed until its own traffic
    /// happened to touch the loop's tail.
    ///
    /// Drives a REAL two-peer `run_one_session` (real Noise handshakes for
    /// both peers) so the malformed frame from PEER_A genuinely decrypts
    /// under ITS OWN completed transport (a Noise-level decrypt failure would
    /// instead hit `Poll::PeerDown`, a different code path this test is not
    /// about) and only fails at the `map_client_frame` (non-JSON plaintext)
    /// step — exactly the "malformed encrypted control frame" the ticket
    /// describes.
    #[tokio::test]
    async fn malformed_frame_does_not_suppress_revocation_broadcast_to_other_peers() {
        let identity = AgentIdentity::generate();
        let agent_kid = identity.key_id();
        let agent_pub = identity.agreement_public_raw();

        let mut allow = HashSet::new();
        allow.insert(CLIENT_KEY_ID.to_string());
        allow.insert(PEER_B.to_string());
        let mut ctx = fresh_ctx(&agent_kid, allow);

        // Pre-arm as CLIENT_KEY_ID (PEER_A) so PEER_B's hello greeting
        // shows `transmitArmed: true`, giving the later broadcast a real
        // true→false transition to prove for PEER_B. Armed on the injected
        // `ctx.clock` timeline (`NOW`) the session loop reads.
        let real_now = NOW;
        ctx.arm
            .lock()
            .unwrap()
            .set_channel_consent(ConsentChannel::StationAgent, true, real_now);
        ctx.arm.lock().unwrap().arm(
            pancetta_agent::arm::VerifiedArmGrant {
                operator_callsign: OPERATOR.to_string(),
                ttl_ms: 3_600_000,
                armed_until_ms: real_now + 3_600_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: "malformed-frame-test-jti".to_string(),
                client_key_id: CLIENT_KEY_ID.to_string(),
            },
            at(real_now),
        );

        let poll_arm = ctx.arm.clone();
        let poll_revocation_pending = ctx.revocation_pending.clone();
        let revocation_pending_after = ctx.revocation_pending.clone();

        let bus = MessageBus::new(64).unwrap();
        let (dev_a, dev_b) = (client_device(), peer_b_device());
        let mut init_a = initiator_for(&dev_a, &agent_pub);
        let mut init_b = initiator_for(&dev_b, &agent_pub);
        let msg1_a = init_a.write_msg1(&proof_for(&dev_a));
        let msg1_b = init_b.write_msg1(&proof_for(&dev_b));

        let hello = RelayFrame::Hello {
            challenge: b64url(&[13u8; 32]),
        }
        .to_json()
        .unwrap();
        let env_a = RelayFrame::Env {
            dst: agent_kid.clone(),
            payload: b64url(&msg1_a),
            src: Some(CLIENT_KEY_ID.to_string()),
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

        let outbound = Arc::new(Mutex::new(Vec::new()));
        // Only the handshake frames are scripted up front — the malformed
        // follow-up from PEER_A can't be pre-built because it must be
        // encrypted under PEER_A's real completed transport, which only
        // exists after the agent's msg2 (emitted mid-session) is observed.
        let mock = MockWs::new(vec![hello, env_a, env_b], outbound.clone());
        let ws_handle = mock.clone();
        // PAN-95: PEER_B receives the revocation broadcast only if it holds a
        // valid hello capability, so right before recv #4 complete its client
        // transport and queue its encrypted real-clock hello (→ greeting,
        // armed=true).
        let hello_ws_handle = mock.clone();
        let hello_outbound = outbound.clone();
        let hello_agent_kid = agent_kid.clone();
        let transport_b_slot: Arc<Mutex<Option<snow::TransportState>>> = Arc::new(Mutex::new(None));
        let slot_b = transport_b_slot.clone();
        let hello_ws = TriggerAfterWs::new(mock, 4, move || {
            let msg2_b = envs_to(&hello_outbound.lock().unwrap(), PEER_B).remove(0);
            init_b.read_msg2(&msg2_b);
            let mut t = init_b.into_transport();
            let token = injected_clock_token(&hello_agent_kid, PEER_B, &["status"]);
            let ct = client_encrypt(&mut t, &hello_frame_bytes(&token));
            *slot_b.lock().unwrap() = Some(t);
            hello_ws_handle.push_inbound(
                RelayFrame::Env {
                    dst: hello_agent_kid.clone(),
                    payload: b64url(&ct),
                    src: Some(PEER_B.to_string()),
                }
                .to_json()
                .unwrap(),
            );
        });
        let trigger_outbound = outbound.clone();
        let trigger_agent_kid = agent_kid.clone();
        // Fires right before recv #5 — after PEER_B's hello was processed.
        let ws = TriggerAfterWs::new(hello_ws, 5, move || {
            // Simulate the concurrent poll disarming the live controller
            // between ticks, exactly like the single-peer broadcast test.
            let effects = poll_arm
                .lock()
                .unwrap()
                .disarm_with_reason(DisarmReason::AuthorizationRevoked, now_ms());
            assert!(
                !effects.is_empty(),
                "setup sanity: the simulated poll disarm must actually find a live arm"
            );
            poll_revocation_pending.store(true, Ordering::Release);

            // Recover PEER_A's msg2 (the first env addressed to CLIENT_KEY_ID
            // so far — sent during env_a's handshake bootstrap, before env_b
            // was processed) to complete a REAL client-side transport for it.
            let out = trigger_outbound.lock().unwrap().clone();
            let msg2_a = out
                .iter()
                .find_map(|s| match parse_frame(s).unwrap() {
                    RelayFrame::Env { dst, payload, .. } if dst == CLIENT_KEY_ID => Some(
                        base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(&payload)
                            .unwrap(),
                    ),
                    _ => None,
                })
                .expect("PEER_A's msg2 must already be on the wire by the 5th tick");
            init_a.read_msg2(&msg2_a);
            let mut client_transport_a = init_a.into_transport();

            // Genuinely non-JSON plaintext — decrypts fine under PEER_A's
            // real transport, but fails `map_client_frame`'s
            // `serde_json::from_slice`, taking the malformed-frame `continue`
            // arm this test targets (not a Noise-level decrypt failure).
            let plaintext = b"not valid json{{{";
            let mut ct = vec![0u8; plaintext.len() + 16];
            let n = client_transport_a
                .write_message(plaintext, &mut ct)
                .unwrap();
            ct.truncate(n);
            let malformed_env = RelayFrame::Env {
                dst: trigger_agent_kid.clone(),
                payload: b64url(&ct),
                src: Some(CLIENT_KEY_ID.to_string()),
            }
            .to_json()
            .unwrap();
            ws_handle.push_inbound(malformed_env);
        });

        run_one_session(ws, &identity, &mut ctx, &bus, &mut None).await;

        assert!(
            ctx.peers.contains_key(CLIENT_KEY_ID),
            "a malformed control frame must not disconnect the established peer \
             that sent it (only a Noise-level decrypt failure would)"
        );
        assert!(
            ctx.peers.contains_key(PEER_B),
            "PEER_B must remain established throughout"
        );
        assert!(
            !revocation_pending_after.load(Ordering::Acquire),
            "revocation_pending must have been cleared by the broadcast"
        );

        // PEER_B never sent anything malformed — its wire traffic must show
        // msg2, the hello greeting (armed=true), and the poll-driven
        // broadcast (armed=false), proving PEER_A's malformed frame did not
        // suppress the broadcast to PEER_B.
        let out = outbound.lock().unwrap().clone();
        let envs_b = envs_to(&out, PEER_B);
        assert_eq!(
            envs_b.len(),
            3,
            "PEER_B: msg2 + hello greeting + the poll-driven broadcast \
             (PAN-34 regression: PEER_A's malformed frame must not swallow this)"
        );
        assert_eq!(
            envs_to(&out, CLIENT_KEY_ID).len(),
            1,
            "PEER_A never sent a hello — it gets its msg2 and nothing else"
        );

        let mut transport_b = transport_b_slot.lock().unwrap().take().unwrap();
        let mut decrypt = |ct: &[u8]| client_decrypt_json(&mut transport_b, ct);

        let greeting = decrypt(&envs_b[1]);
        assert_eq!(greeting["event"]["event"], "controlState");
        assert_eq!(
            greeting["event"]["transmitArmed"], true,
            "the hello greeting must reflect the pre-armed state"
        );

        let broadcast = decrypt(&envs_b[2]);
        assert_eq!(broadcast["event"]["event"], "controlState");
        assert_eq!(
            broadcast["event"]["transmitArmed"], false,
            "the poll-driven disarm must still broadcast to PEER_B even while \
             PEER_A is stuck resending a malformed frame"
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
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
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
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
                at(NOW),
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

    // ── PAN-96: live authority checks on every use ──────────────────────────

    /// Drop `peer` from the live connection-admission list, as a successful
    /// authorization poll that revoked it would.
    fn revoke_admission(ctx: &ArmContext, peer: &str) {
        ctx.tx_allow_list.write().unwrap().remove(peer);
    }

    /// The `Qsy` used by the PAN-96 tests.
    fn qsy_action() -> ControlAction {
        ControlAction::Qsy {
            vfo: 0,
            frequency_hz: 14_074_000.0,
        }
    }

    /// The `SetSplit` used by the PAN-96 tests.
    fn split_action() -> ControlAction {
        ControlAction::SetSplit {
            enabled: true,
            tx_frequency_hz: 14_075_000.0,
        }
    }

    /// Ticket evidence `security_review_expired_hello_still_controls_rig`: a
    /// hello token verified as expired must not yield a `SetFrequency` (or a
    /// `SetSplit`) from dispatch on the still-connected socket.
    #[tokio::test]
    async fn expired_hello_capability_cannot_qsy_or_split() {
        let mut ctx = ctx_with(true, true);
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let tok = token_expiring_at(NOW / 1000 + 5);
        let hello = ControlAction::Hello {
            capability_token: Some(tok.clone()),
        };
        dispatch_action(hello, CLIENT_KEY_ID, &mut ctx, &bus, SESSION_ID, at(NOW)).await;
        assert!(
            ctx.peers[CLIENT_KEY_ID].auth.is_some(),
            "setup: the hello verified at NOW"
        );
        let later = NOW + 60_000;
        assert!(
            matches!(
                ctx.verifier.verify_capability_token(&tok, later),
                Err(pancetta_agent::capability::CapError::Expired)
            ),
            "setup: the same token verifies as expired a minute later"
        );
        dispatch_action(
            qsy_action(),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(later),
        )
        .await;
        dispatch_action(
            split_action(),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(later),
        )
        .await;
        assert!(
            hamlib_rx.try_recv().is_err(),
            "an expired hello capability must forward no RigControl message"
        );
    }

    #[tokio::test]
    async fn qsy_refused_once_peer_is_dropped_from_admission_list() {
        let mut ctx = ctx_with(true, true);
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let hello = ControlAction::Hello {
            capability_token: Some(valid_token()),
        };
        dispatch_action(hello, CLIENT_KEY_ID, &mut ctx, &bus, SESSION_ID, at(NOW)).await;
        dispatch_action(
            qsy_action(),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(
            hamlib_rx.try_recv().is_ok(),
            "setup: qsy permitted while admitted"
        );

        revoke_admission(&ctx, CLIENT_KEY_ID);
        dispatch_action(
            qsy_action(),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(
            hamlib_rx.try_recv().is_err(),
            "a peer dropped from the admission list must not drive setFrequency"
        );
    }

    #[tokio::test]
    async fn set_split_refused_once_peer_is_dropped_from_admission_list() {
        let mut ctx = ctx_with(true, true);
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let hello = ControlAction::Hello {
            capability_token: Some(valid_token()),
        };
        dispatch_action(hello, CLIENT_KEY_ID, &mut ctx, &bus, SESSION_ID, at(NOW)).await;
        dispatch_action(
            split_action(),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(
            hamlib_rx.try_recv().is_ok(),
            "setup: setSplit permitted while admitted"
        );

        revoke_admission(&ctx, CLIENT_KEY_ID);
        dispatch_action(
            split_action(),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(
            hamlib_rx.try_recv().is_err(),
            "a peer dropped from the admission list must not drive setSplit"
        );
    }

    /// An armed controller with a hello capability, then dropped from the
    /// admission list. Returns the context and the bus receiver for Hamlib.
    async fn armed_controller_then_revoked(
        bus: &MessageBus,
    ) -> (ArmContext, crossbeam_channel::Receiver<ComponentMessage>) {
        let mut ctx = ctx_with(true, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let hello = ControlAction::Hello {
            capability_token: Some(valid_token()),
        };
        dispatch_action(hello, CLIENT_KEY_ID, &mut ctx, bus, SESSION_ID, at(NOW)).await;
        dispatch_action(
            arm_action("revoked-arm-jti"),
            CLIENT_KEY_ID,
            &mut ctx,
            bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(
            ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW), ConsentChannel::StationAgent),
            "setup: armed"
        );
        assert_eq!(ctx.controller.as_deref(), Some(CLIENT_KEY_ID));
        revoke_admission(&ctx, CLIENT_KEY_ID);
        (ctx, hamlib_rx)
    }

    #[tokio::test]
    async fn revoked_peer_can_only_disarm_or_release() {
        let bus = MessageBus::new(64).unwrap();
        let (mut ctx, hamlib_rx) = armed_controller_then_revoked(&bus).await;
        let later = NOW + 20_000;
        let refused = [
            ControlAction::TakeControl,
            ControlAction::Hello {
                // A different, valid token: were it honored, `auth.jti` would change.
                capability_token: Some(token_expiring_at(NOW / 1000 + 300)),
            },
            ControlAction::Heartbeat {
                arm_jti: "revoked-arm-jti".into(),
                seq: 1,
            },
            ControlAction::StopCq,
            qsy_action(),
        ];
        for action in refused {
            let label = format!("{action:?}");
            let out =
                dispatch_action(action, CLIENT_KEY_ID, &mut ctx, &bus, SESSION_ID, at(later)).await;
            assert!(out.sends.is_empty(), "{label}: no frames");
            assert_eq!(
                ctx.controller.as_deref(),
                Some(CLIENT_KEY_ID),
                "{label}: controller unchanged"
            );
            assert!(hamlib_rx.try_recv().is_err(), "{label}: no bus message");
            assert_eq!(
                ctx.peers[CLIENT_KEY_ID].auth.as_ref().unwrap().jti,
                "cap-jti-1",
                "{label}: the cached capability is not refreshed"
            );
        }
        assert!(
            !ctx.arm.lock().unwrap().tx_permitted(
                at(later + HEARTBEAT_TIMEOUT_MS - 1),
                ConsentChannel::StationAgent
            ),
            "the refused heartbeat must not have slid the dead-man window"
        );

        // Disarm still works: it only reduces authority.
        dispatch_action(
            ControlAction::Disarm {
                arm_jti: String::new(),
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW + 1_000),
        )
        .await;
        assert!(
            !ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW + 1_000), ConsentChannel::StationAgent),
            "a revoked peer may still disarm"
        );

        // ReleaseControl still works too.
        let bus = MessageBus::new(64).unwrap();
        let (mut ctx, _rx) = armed_controller_then_revoked(&bus).await;
        dispatch_action(
            ControlAction::ReleaseControl,
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW + 1_000),
        )
        .await;
        assert!(
            ctx.controller.is_none(),
            "a revoked peer may still release control"
        );
        assert!(!ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(NOW + 1_000), ConsentChannel::StationAgent));
    }

    #[tokio::test]
    async fn revoked_peer_cannot_grab_control() {
        let mut ctx = ctx_with(true, true);
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let hello = ControlAction::Hello {
            capability_token: Some(valid_token()),
        };
        dispatch_action(hello, CLIENT_KEY_ID, &mut ctx, &bus, SESSION_ID, at(NOW)).await;
        revoke_admission(&ctx, CLIENT_KEY_ID);
        assert!(ctx.controller.is_none());
        dispatch_action(
            qsy_action(),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(
            ctx.controller.is_none(),
            "a revoked peer's qsy must not implicitly grab control"
        );
        assert!(hamlib_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn outbound_withheld_once_peer_is_dropped_from_admission_list() {
        let identity = AgentIdentity::generate();
        let mut allow = HashSet::new();
        allow.insert(CLIENT_KEY_ID.to_string());
        allow.insert(PEER_B.to_string());
        let mut ctx = fresh_ctx(&identity.key_id(), allow);
        let (dev_a, dev_b) = (client_device(), peer_b_device());
        let (mut sess, outbound, mut transports) = hand_session(&identity, &[&dev_a, &dev_b]);
        for p in [CLIENT_KEY_ID, PEER_B] {
            ctx.peers.insert(
                p.to_string(),
                PeerCtx::authorized_until(vec!["status".to_string()], NOW + 60_000),
            );
        }
        revoke_admission(&ctx, CLIENT_KEY_ID);

        let before_a = envs_to(&outbound.lock().unwrap(), CLIENT_KEY_ID).len();
        let before = outbound.lock().unwrap().len();
        let mut events = feed_with(pancetta_protocol::ServerEvent::TxStatus { active: true });
        drain_read_stream(&mut events, &mut sess, &ctx, || NOW);
        let out = outbound.lock().unwrap().clone();
        assert!(
            envs_to(&out[before..], CLIENT_KEY_ID).is_empty(),
            "no read-stream event to the dropped peer"
        );
        let envs_b = envs_to(&out[before..], PEER_B);
        assert_eq!(envs_b.len(), 1, "B still gets the event");
        let v = client_decrypt_json(&mut transports[1], &envs_b[0]);
        assert_eq!(v["event"]["event"], "txStatus");

        let sends = control_state_sends(&ctx, at(NOW), None);
        let before = outbound.lock().unwrap().len();
        deliver_sends(&mut sess, &ctx, &sends, || NOW);
        let out = outbound.lock().unwrap().clone();
        assert_eq!(
            envs_to(&out[before..], PEER_B).len(),
            1,
            "B still gets controlState"
        );
        assert_eq!(
            envs_to(&out, CLIENT_KEY_ID).len(),
            before_a,
            "the dropped peer is sent nothing at all"
        );
    }

    #[tokio::test]
    async fn hello_with_deny_listed_jti_grants_nothing() {
        let mut ctx = ctx_with(true, true);
        let token = injected_clock_token(AGENT_KEY_ID, CLIENT_KEY_ID, &["status", "qsy"]);
        assert!(
            dispatch_hello(Some(token.clone()), CLIENT_KEY_ID, &mut ctx, NOW).0,
            "setup: the token is otherwise valid"
        );
        ctx.revoked_jtis.insert(format!("hello-{CLIENT_KEY_ID}"));
        assert!(
            !dispatch_hello(Some(token), CLIENT_KEY_ID, &mut ctx, NOW).0,
            "a deny-listed token must not re-authorize"
        );
        assert!(
            ctx.peers[CLIENT_KEY_ID].auth.is_none(),
            "the refused hello clears any prior capability"
        );
    }

    /// Round-2 review (PR #426): an armed controller's REFUSED hello — here the
    /// legacy `hello` with no capabilityToken — erases a still-unexpired
    /// capability. The expiry sweep cannot see that lapse (the old `exp_ms` is
    /// erased unexpired) and heartbeats check only the arm's own jti, so the
    /// clear must be treated like expiry (`expire_peer_auth`): disarm, release
    /// control, and return the updated controlState for the remaining peers.
    #[tokio::test]
    async fn refused_hello_releases_the_armed_controller() {
        let mut ctx = ctx_with(true, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        let bus = MessageBus::new(64).unwrap();
        // The client holds a live, unexpired capability and is the armed
        // controller.
        ctx.peers.insert(
            CLIENT_KEY_ID.to_string(),
            PeerCtx::authorized_until(vec!["tx".to_string()], NOW + 600_000),
        );
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(
            ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW), ConsentChannel::StationAgent),
            "setup: armed"
        );
        assert_eq!(ctx.controller.as_deref(), Some(CLIENT_KEY_ID));

        let out = dispatch_action(
            ControlAction::Hello {
                capability_token: None,
            },
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;

        assert_eq!(out.flow, Dispatch::Continue);
        assert!(
            ctx.peers[CLIENT_KEY_ID].auth.is_none(),
            "the refused hello cleared the prior capability"
        );
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "clearing the controller's capability must disarm, like expiry"
        );
        assert_eq!(
            ctx.controller, None,
            "clearing the controller's capability must release control, like expiry"
        );
        assert!(
            out.sends.iter().any(|s| matches!(
                &s.frame,
                pancetta_protocol::ServerFrame::Event {
                    event: pancetta_protocol::ServerEvent::ControlState { .. }
                }
            )),
            "the updated controlState is returned for the remaining peers"
        );
    }

    /// PR #425 review: a peer-matched, otherwise-valid hello token whose scopes
    /// hold nothing at `status` or above (empty, or only unrecognized strings)
    /// must be a FAILED hello. Storing it would leave a future-dated
    /// `Some(auth)` that `peer_may_receive` refuses but the expiry sweep never
    /// flags, so an armed controller could keep heartbeating and transmitting
    /// while the station sends it nothing. It must clear the capability,
    /// disarm, and release control.
    #[tokio::test]
    async fn hello_without_status_scope_fails_and_releases_the_armed_controller() {
        for scopes in [&[][..], &["bogus"][..]] {
            let mut ctx = ctx_with(true, true);
            with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
            let bus = MessageBus::new(64).unwrap();
            ctx.peers.insert(
                CLIENT_KEY_ID.to_string(),
                PeerCtx::authorized_until(vec!["tx".to_string()], NOW + 600_000),
            );
            dispatch_action(
                arm_action("arm-jti-1"),
                CLIENT_KEY_ID,
                &mut ctx,
                &bus,
                SESSION_ID,
                at(NOW),
            )
            .await;
            assert!(ctx.arm.lock().unwrap().is_armed(), "setup: armed");
            assert_eq!(ctx.controller.as_deref(), Some(CLIENT_KEY_ID));

            let token = injected_clock_token(AGENT_KEY_ID, CLIENT_KEY_ID, scopes);
            let out = dispatch_action(
                ControlAction::Hello {
                    capability_token: Some(token),
                },
                CLIENT_KEY_ID,
                &mut ctx,
                &bus,
                SESSION_ID,
                at(NOW),
            )
            .await;

            assert!(
                ctx.peers[CLIENT_KEY_ID].auth.is_none(),
                "scopes {scopes:?}: a hello without status scope must not be stored"
            );
            assert!(
                !ctx.arm.lock().unwrap().is_armed(),
                "scopes {scopes:?}: it must disarm the controller"
            );
            assert_eq!(
                ctx.controller, None,
                "scopes {scopes:?}: and release control"
            );
            assert!(
                out.sends.iter().any(|s| matches!(
                    &s.frame,
                    pancetta_protocol::ServerFrame::Event {
                        event: pancetta_protocol::ServerEvent::ControlState { .. }
                    }
                )),
                "scopes {scopes:?}: the updated controlState is returned for the remaining peers"
            );
        }

        // A direct dispatch_hello reports the refusal too.
        let mut ctx = ctx_with(true, true);
        let token = injected_clock_token(AGENT_KEY_ID, CLIENT_KEY_ID, &[]);
        assert!(!dispatch_hello(Some(token), CLIENT_KEY_ID, &mut ctx, NOW).0);
    }

    #[tokio::test]
    async fn hello_stores_token_jti() {
        let mut ctx = ctx_with(true, true);
        let token = injected_clock_token(AGENT_KEY_ID, CLIENT_KEY_ID, &["status"]);
        assert!(dispatch_hello(Some(token), CLIENT_KEY_ID, &mut ctx, NOW).0);
        assert_eq!(
            ctx.peers[CLIENT_KEY_ID].auth.as_ref().unwrap().jti,
            format!("hello-{CLIENT_KEY_ID}")
        );
    }

    #[tokio::test]
    async fn peer_admitted_fails_closed_on_poisoned_lock() {
        let mut ctx = ctx_with(true, true);
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let hello = ControlAction::Hello {
            capability_token: Some(valid_token()),
        };
        dispatch_action(hello, CLIENT_KEY_ID, &mut ctx, &bus, SESSION_ID, at(NOW)).await;
        assert!(peer_admitted(&ctx, CLIENT_KEY_ID), "setup: admitted");

        let list = ctx.tx_allow_list.clone();
        let _ = std::thread::spawn(move || {
            let _guard = list.write().unwrap();
            panic!("poison the admission list");
        })
        .join();
        assert!(ctx.tx_allow_list.is_poisoned(), "setup: poisoned");

        assert!(
            !peer_admitted(&ctx, CLIENT_KEY_ID),
            "a poisoned admission list admits nobody"
        );
        dispatch_action(
            qsy_action(),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(hamlib_rx.try_recv().is_err(), "and authorizes nothing");
    }

    // ── PAN-96: authorization generation + reconcile/evict ──────────────────

    /// A cqdx mock serving `body` (which the test may swap) at the
    /// authorizations endpoint and counting the polls it serves.
    async fn authorizations_mock(
        body: Arc<Mutex<Value>>,
        served: Arc<std::sync::atomic::AtomicUsize>,
    ) -> std::net::SocketAddr {
        let app = axum::Router::new().route(
            "/api/v1/authorizations",
            axum::routing::get(move || {
                let body = body.clone();
                let served = served.clone();
                async move {
                    served.fetch_add(1, Ordering::SeqCst);
                    let v = body.lock().unwrap().clone();
                    axum::Json(v)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    /// An owner (non-delegated) edge for `client_kid` on agent `agent_this`.
    fn owner_edge(client_kid: &str) -> Value {
        json!({
            "id": format!("auth_{client_kid}"),
            "agentKeyId": "agent_this",
            "clientKeyId": client_kid,
            "scopes": ["status", "qsy"],
            "createdAt": "2026-08-15T00:00:00Z",
            "role": "owner"
        })
    }

    fn cqdx_cfg_at(base_url: String) -> pancetta_config::network::CqdxConfig {
        pancetta_config::network::CqdxConfig {
            enabled: true,
            base_url,
            token: Some("pat_test_token_0000000000".to_string()),
            poll_interval_secs: 30,
            authorizations_poll_interval_secs: 45,
        }
    }

    /// Poll `cond` every 5 ms for up to 2 s; returns its final value.
    async fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..400 {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cond()
    }

    /// Spawn the real poll loop over `ctx`'s shared lists, arm, audit and
    /// generation, polling `base_url` every 10 ms.
    fn spawn_poll_for(
        ctx: &ArmContext,
        base_url: String,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(poll_authorizations_loop(
            cqdx_cfg_at(base_url),
            "agent_this".to_string(),
            std::env::temp_dir(),
            ctx.tx_allow_list.clone(),
            ctx.tx_arm_allow_list.clone(),
            ctx.client_keys.clone(),
            ctx.arm.clone(),
            ctx.audit.clone(),
            ctx.revocation_pending.clone(),
            shutdown,
            Duration::from_millis(10),
            ctx.authz_generation.clone(),
        ))
    }

    /// Bump the authorization generation, as a list-changing poll does.
    fn bump_generation(ctx: &ArmContext) {
        ctx.authz_generation.fetch_add(1, Ordering::AcqRel);
    }

    /// An established peer holding a `status`+`qsy` hello capability whose
    /// `jti` is `hello-<kid>` (what `injected_clock_token` mints).
    fn hello_authorized(kid: &str, exp_ms: i64) -> PeerCtx {
        PeerCtx {
            auth: Some(PeerAuth {
                scopes: vec!["status".to_string(), "qsy".to_string()],
                exp_ms,
                jti: format!("hello-{kid}"),
            }),
        }
    }

    /// A (CLIENT_KEY_ID) and B (PEER_B) both admitted, established over real
    /// handshakes, and hello-authorized until `NOW + 60_000`.
    #[allow(clippy::type_complexity)]
    fn two_authorized_peers(
        identity: &AgentIdentity,
    ) -> (
        ArmContext,
        MultiPeerSession<'_, MockWs>,
        Arc<Mutex<Vec<String>>>,
        Vec<snow::TransportState>,
        MockWs,
    ) {
        let allow = HashSet::from([CLIENT_KEY_ID.to_string(), PEER_B.to_string()]);
        let mut ctx = fresh_ctx(&identity.key_id(), allow);
        let (sess, outbound, transports, ws) =
            hand_session_ws(identity, &[&client_device(), &peer_b_device()]);
        for kid in [CLIENT_KEY_ID, PEER_B] {
            ctx.peers
                .insert(kid.to_string(), hello_authorized(kid, NOW + 60_000));
        }
        (ctx, sess, outbound, transports, ws)
    }

    /// The relay `env` carrying `payload` from `src` to `agent_kid`.
    fn env_from(agent_kid: &str, src: &str, payload: &[u8]) -> String {
        RelayFrame::Env {
            dst: agent_kid.to_string(),
            payload: b64url(payload),
            src: Some(src.to_string()),
        }
        .to_json()
        .unwrap()
    }

    /// Every `Disarmed` record in `ctx`'s audit log.
    fn disarmed_records(ctx: &ArmContext) -> Vec<AuditEvent> {
        std::fs::read_to_string(ctx.audit.path())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<AuditEvent>(l).ok())
            .filter(|ev| ev.kind == AuditKind::Disarmed)
            .collect()
    }

    /// Arm `ctx` live for `kid` at `NOW` and make it the controller.
    fn arm_controller(ctx: &mut ArmContext, kid: &str) {
        let mut st = ctx.arm.lock().unwrap();
        st.set_channel_consent(ConsentChannel::StationAgent, true, NOW);
        st.arm(
            pancetta_agent::arm::VerifiedArmGrant {
                operator_callsign: OPERATOR.to_string(),
                ttl_ms: 300_000,
                armed_until_ms: NOW + 300_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: "reconcile-arm-jti".to_string(),
                client_key_id: kid.to_string(),
            },
            at(NOW),
        );
        assert!(
            st.tx_permitted(at(NOW), ConsentChannel::StationAgent),
            "setup: armed"
        );
        drop(st);
        ctx.controller = Some(kid.to_string());
    }

    #[tokio::test]
    async fn poll_authorizations_loop_bumps_generation_only_when_lists_change() {
        let body = Arc::new(Mutex::new(
            json!({ "authorizations": [owner_edge(CLIENT_KEY_ID)] }),
        ));
        let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = authorizations_mock(body.clone(), served.clone()).await;
        let ctx = fresh_ctx(AGENT_KEY_ID, HashSet::from([CLIENT_KEY_ID.to_string()]));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = spawn_poll_for(&ctx, format!("http://{addr}"), shutdown.clone());
        let generation = ctx.authz_generation.clone();

        assert!(
            wait_until(|| served.load(Ordering::SeqCst) >= 3).await,
            "setup: the mock served at least 3 polls"
        );
        assert_eq!(
            generation.load(Ordering::Acquire),
            0,
            "polls that change neither list bump nothing"
        );

        *body.lock().unwrap() = json!({ "authorizations": [] });
        assert!(
            wait_until(|| generation.load(Ordering::Acquire) >= 1).await,
            "a poll that changes the lists bumps the generation"
        );
        assert!(ctx.tx_allow_list.read().unwrap().is_empty());
        assert!(ctx.tx_arm_allow_list.read().unwrap().is_empty());

        let after = served.load(Ordering::SeqCst);
        assert!(wait_until(|| served.load(Ordering::SeqCst) >= after + 3).await);
        assert_eq!(
            generation.load(Ordering::Acquire),
            1,
            "further unchanged polls bump nothing"
        );
        shutdown.store(true, Ordering::Release);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn poll_failure_never_bumps_generation() {
        let ctx = fresh_ctx(AGENT_KEY_ID, HashSet::from(["seed".to_string()]));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Nothing listens on port 1: every poll fails.
        let handle = spawn_poll_for(&ctx, "http://127.0.0.1:1".to_string(), shutdown.clone());
        tokio::time::sleep(Duration::from_millis(150)).await;
        shutdown.store(true, Ordering::Release);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert_eq!(
            ctx.authz_generation.load(Ordering::Acquire),
            0,
            "a failed poll publishes nothing and bumps nothing"
        );
        assert_eq!(
            *ctx.tx_allow_list.read().unwrap(),
            HashSet::from(["seed".to_string()])
        );
    }

    #[tokio::test]
    async fn reconcile_is_a_noop_when_generation_unchanged() {
        let identity = AgentIdentity::generate();
        let (mut ctx, mut sess, _out, _t, _ws) = two_authorized_peers(&identity);
        revoke_admission(&ctx, CLIENT_KEY_ID);

        assert!(reconcile_authorizations(&mut ctx, &mut sess, at(NOW)).is_empty());
        assert_eq!(sess.established_peers().count(), 2, "nobody evicted");
        assert!(ctx.peers.contains_key(CLIENT_KEY_ID));
        assert!(ctx.peers.contains_key(PEER_B));
        assert!(ctx.revoked_jtis.is_empty());
        assert!(
            !peer_may_receive(&ctx, CLIENT_KEY_ID, NOW),
            "the live admission check still refuses A before any reconcile"
        );
    }

    #[tokio::test]
    async fn reconcile_evicts_revoked_peer_and_deny_lists_its_token() {
        let identity = AgentIdentity::generate();
        let (mut ctx, mut sess, outbound, mut transports, ws) = two_authorized_peers(&identity);
        revoke_admission(&ctx, CLIENT_KEY_ID);
        bump_generation(&ctx);

        let sends = reconcile_authorizations(&mut ctx, &mut sess, at(NOW));
        assert!(
            sends.is_empty(),
            "no controller was evicted: nothing to tell"
        );
        let established: Vec<&str> = sess.established_peers().collect();
        assert_eq!(established, vec![PEER_B], "A evicted from the demux");
        assert!(
            !ctx.peers.contains_key(CLIENT_KEY_ID),
            "A's PeerCtx removed"
        );
        assert!(
            ctx.revoked_jtis.contains(&format!("hello-{CLIENT_KEY_ID}")),
            "A's hello token is deny-listed"
        );
        assert!(ctx.peers[PEER_B].auth.is_some(), "B untouched");
        assert_eq!(ctx.authz_generation_seen, 1);

        // A's next frame, under its old transport, is refused with no reply.
        let before = outbound.lock().unwrap().len();
        let ct = client_encrypt(&mut transports[0], &hello_frame_bytes("stale"));
        ws.push_inbound(env_from(&identity.key_id(), CLIENT_KEY_ID, &ct));
        assert_eq!(
            sess.process_next(Duration::from_millis(1)).unwrap(),
            Poll::PeerRefused {
                peer: CLIENT_KEY_ID.to_string(),
                reason: pancetta_agent::multi_session::RefuseReason::NotAllowListed,
            }
        );
        assert_eq!(outbound.lock().unwrap().len(), before, "no reply");
    }

    #[tokio::test]
    async fn reconcile_evicting_the_controller_disarms_and_releases_control() {
        let identity = AgentIdentity::generate();
        let (mut ctx, mut sess, outbound, mut transports, _ws) = two_authorized_peers(&identity);
        arm_controller(&mut ctx, CLIENT_KEY_ID);
        let disarmed_before = disarmed_records(&ctx).len();
        revoke_admission(&ctx, CLIENT_KEY_ID);
        bump_generation(&ctx);

        let sends = reconcile_authorizations(&mut ctx, &mut sess, at(NOW));
        assert_eq!(ctx.controller, None, "a revoked controller is released");
        assert!(
            !ctx.arm
                .lock()
                .unwrap()
                .tx_permitted(at(NOW), ConsentChannel::StationAgent),
            "and disarmed"
        );
        let disarmed = disarmed_records(&ctx);
        assert_eq!(
            disarmed.len(),
            disarmed_before + 1,
            "exactly one Disarmed record"
        );
        assert!(disarmed
            .last()
            .unwrap()
            .detail
            .contains("authorization-revoked"));
        assert!(!sends.is_empty(), "the remaining peers are told");
        assert!(
            sends
                .iter()
                .all(|s| matches!(&s.to, SendTarget::One(p) if p == PEER_B)),
            "only B is addressed: {sends:?}"
        );
        let before = outbound.lock().unwrap().len();
        deliver_sends(&mut sess, &ctx, &sends, || NOW);
        let out = outbound.lock().unwrap().clone();
        let envs_b = envs_to(&out[before..], PEER_B);
        assert_eq!(envs_b.len(), 1);
        let v = client_decrypt_json(&mut transports[1], &envs_b[0]);
        assert_eq!(v["event"]["event"], "controlState");
        assert_eq!(v["event"]["transmitArmed"], false);
        assert_eq!(v["event"]["controlHeldByMe"], false);

        // The poll task already disarmed: reconcile still releases control
        // and writes no second Disarmed record.
        let identity = AgentIdentity::generate();
        let (mut ctx, mut sess, _out, _t, _ws) = two_authorized_peers(&identity);
        arm_controller(&mut ctx, CLIENT_KEY_ID);
        let effects = ctx
            .arm
            .lock()
            .unwrap()
            .disarm_with_reason(DisarmReason::AuthorizationRevoked, NOW);
        apply_arm_effects(&ctx.audit, &effects);
        let disarmed_before = disarmed_records(&ctx).len();
        assert_eq!(disarmed_before, 1, "setup: the poll task's disarm");
        revoke_admission(&ctx, CLIENT_KEY_ID);
        bump_generation(&ctx);
        reconcile_authorizations(&mut ctx, &mut sess, at(NOW));
        assert_eq!(ctx.controller, None);
        assert_eq!(
            disarmed_records(&ctx).len(),
            disarmed_before,
            "no duplicate Disarmed record"
        );
    }

    #[tokio::test]
    async fn reconcile_admits_newly_listed_client_mid_session() {
        let identity = AgentIdentity::generate();
        let agent_kid = identity.key_id();
        let mut ctx = fresh_ctx(&agent_kid, HashSet::from([CLIENT_KEY_ID.to_string()]));
        let (mut sess, _out, _t, ws) = hand_session_ws(&identity, &[&client_device()]);
        let dev_c = peer_b_device();
        let msg1_from_c = || {
            let mut init = initiator_for(&dev_c, &identity.agreement_public_raw());
            env_from(&agent_kid, PEER_B, &init.write_msg1(&proof_for(&dev_c)))
        };

        ws.push_inbound(msg1_from_c());
        assert_eq!(
            sess.process_next(Duration::from_millis(1)).unwrap(),
            Poll::PeerRefused {
                peer: PEER_B.to_string(),
                reason: pancetta_agent::multi_session::RefuseReason::NotAllowListed,
            },
            "setup: C is not admitted yet"
        );

        ctx.tx_allow_list
            .write()
            .unwrap()
            .insert(PEER_B.to_string());
        bump_generation(&ctx);
        assert!(reconcile_authorizations(&mut ctx, &mut sess, at(NOW)).is_empty());

        ws.push_inbound(msg1_from_c());
        assert!(
            matches!(
                sess.process_next(Duration::from_millis(1)).unwrap(),
                Poll::PeerEstablished { ref peer, .. } if peer == PEER_B
            ),
            "a newly authorized client establishes without a reconnect"
        );
        assert!(sess.session_id(CLIENT_KEY_ID).is_some(), "A untouched");
    }

    #[tokio::test]
    async fn revoked_peer_must_reauthenticate_after_readmission() {
        let identity = AgentIdentity::generate();
        let agent_kid = identity.key_id();
        let mut ctx = fresh_ctx(&agent_kid, HashSet::from([CLIENT_KEY_ID.to_string()]));
        let (mut sess, _out, mut transports, ws) = hand_session_ws(&identity, &[&client_device()]);
        ctx.peers
            .insert(CLIENT_KEY_ID.to_string(), PeerCtx::unauthorized());
        let old_token = injected_clock_token(&agent_kid, CLIENT_KEY_ID, &["status", "qsy"]);
        assert!(dispatch_hello(Some(old_token.clone()), CLIENT_KEY_ID, &mut ctx, NOW).0);

        // Revoked and evicted.
        revoke_admission(&ctx, CLIENT_KEY_ID);
        bump_generation(&ctx);
        reconcile_authorizations(&mut ctx, &mut sess, at(NOW));
        assert!(!ctx.peers.contains_key(CLIENT_KEY_ID));

        // Re-authorized.
        ctx.tx_allow_list
            .write()
            .unwrap()
            .insert(CLIENT_KEY_ID.to_string());
        bump_generation(&ctx);
        assert!(reconcile_authorizations(&mut ctx, &mut sess, at(NOW)).is_empty());

        // The old transport cannot resume: its frame is not a msg1.
        let ct = client_encrypt(&mut transports[0], &hello_frame_bytes(&old_token));
        ws.push_inbound(env_from(&agent_kid, CLIENT_KEY_ID, &ct));
        assert_eq!(
            sess.process_next(Duration::from_millis(1)).unwrap(),
            Poll::PeerRefused {
                peer: CLIENT_KEY_ID.to_string(),
                reason: pancetta_agent::multi_session::RefuseReason::Handshake,
            }
        );

        // A fresh handshake establishes it with no capability.
        let dev_a = client_device();
        let mut init = initiator_for(&dev_a, &identity.agreement_public_raw());
        ws.push_inbound(env_from(
            &agent_kid,
            CLIENT_KEY_ID,
            &init.write_msg1(&proof_for(&dev_a)),
        ));
        match sess.process_next(Duration::from_millis(1)).unwrap() {
            // What `run_one_session` does on establishment.
            Poll::PeerEstablished { peer, .. } => {
                ctx.peers.insert(peer, PeerCtx::unauthorized());
            }
            other => panic!("expected PeerEstablished, got {other:?}"),
        }
        assert!(!peer_may_receive(&ctx, CLIENT_KEY_ID, NOW));

        // The old token stays refused; a freshly minted one is accepted.
        assert!(!dispatch_hello(Some(old_token), CLIENT_KEY_ID, &mut ctx, NOW).0);
        assert!(ctx.peers[CLIENT_KEY_ID].auth.is_none());
        let fresh =
            injected_clock_token_with_jti(&agent_kid, CLIENT_KEY_ID, &["status"], "hello-fresh");
        assert!(dispatch_hello(Some(fresh), CLIENT_KEY_ID, &mut ctx, NOW).0);
        assert!(peer_may_receive(&ctx, CLIENT_KEY_ID, NOW));
    }

    #[tokio::test]
    async fn real_poll_revocation_stops_qsy_split_and_reads() {
        let identity = AgentIdentity::generate();
        let agent_kid = identity.key_id();
        let mut ctx = fresh_ctx(&agent_kid, HashSet::from([CLIENT_KEY_ID.to_string()]));
        let (mut sess, outbound, _t) = hand_session(&identity, &[&client_device()]);
        ctx.peers
            .insert(CLIENT_KEY_ID.to_string(), PeerCtx::unauthorized());
        let token = injected_clock_token(&agent_kid, CLIENT_KEY_ID, &["status", "qsy"]);
        assert!(dispatch_hello(Some(token), CLIENT_KEY_ID, &mut ctx, NOW).0);
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

        // The real poll loop sees the client's authorization revoked.
        let body = Arc::new(Mutex::new(json!({ "authorizations": [] })));
        let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = authorizations_mock(body, served).await;
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = spawn_poll_for(&ctx, format!("http://{addr}"), shutdown.clone());
        let generation = ctx.authz_generation.clone();
        assert!(
            wait_until(|| generation.load(Ordering::Acquire) >= 1).await,
            "the poll published the revocation"
        );
        shutdown.store(true, Ordering::Release);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(!ctx.tx_allow_list.read().unwrap().contains(CLIENT_KEY_ID));

        reconcile_authorizations(&mut ctx, &mut sess, at(NOW));

        let now = NOW;
        dispatch_action(
            qsy_action(),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(now),
        )
        .await;
        dispatch_action(
            split_action(),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(now),
        )
        .await;
        assert!(
            hamlib_rx.try_recv().is_err(),
            "no QSY or split after revocation"
        );
        let before = envs_to(&outbound.lock().unwrap(), CLIENT_KEY_ID).len();
        let mut events = feed_with(pancetta_protocol::ServerEvent::TxStatus { active: true });
        drain_read_stream(&mut events, &mut sess, &ctx, || NOW);
        assert_eq!(
            envs_to(&outbound.lock().unwrap(), CLIENT_KEY_ID).len(),
            before,
            "no read-stream event after revocation"
        );
        assert!(
            !sess.established_peers().any(|p| p == CLIENT_KEY_ID),
            "evicted from the demux"
        );
        assert!(!ctx.peers.contains_key(CLIENT_KEY_ID), "PeerCtx removed");
    }

    /// Gherkin Scenario 2 end-to-end through `run_one_session`: A and B are
    /// connected and hello-authorized; a refresh drops A (as the poll task
    /// would). A's next QSY reaches nothing, A is sent nothing more, and its
    /// session state is gone, while B keeps receiving the display stream.
    #[tokio::test]
    async fn revoking_a_connected_client_evicts_its_live_session() {
        let identity = AgentIdentity::generate();
        let agent_kid = identity.key_id();
        let agent_pub = identity.agreement_public_raw();
        let allow = HashSet::from([CLIENT_KEY_ID.to_string(), PEER_B.to_string()]);
        let mut ctx = fresh_ctx(&agent_kid, allow);
        let bus = MessageBus::new(64).unwrap();
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

        let (dev_a, dev_b) = (client_device(), peer_b_device());
        let mut init_a = initiator_for(&dev_a, &agent_pub);
        let mut init_b = initiator_for(&dev_b, &agent_pub);
        let msg1_a = init_a.write_msg1(&proof_for(&dev_a));
        let msg1_b = init_b.write_msg1(&proof_for(&dev_b));
        let relay_hello = RelayFrame::Hello {
            challenge: b64url(&[23u8; 32]),
        }
        .to_json()
        .unwrap();
        let idle_tick = || {
            RelayFrame::Presence {
                peer: "irrelevant-peer".to_string(),
                state: "up".to_string(),
            }
            .to_json()
            .unwrap()
        };

        let outbound = Arc::new(Mutex::new(Vec::new()));
        let mock = MockWs::new(
            vec![
                relay_hello,
                env_from(&agent_kid, CLIENT_KEY_ID, &msg1_a),
                env_from(&agent_kid, PEER_B, &msg1_b),
            ],
            outbound.clone(),
        );
        let (evt_tx, evt_rx) = tokio::sync::broadcast::channel(16);
        let mut events = Some(evt_rx);
        let slot_a: Arc<Mutex<Option<snow::TransportState>>> = Arc::new(Mutex::new(None));
        let slot_b: Arc<Mutex<Option<snow::TransportState>>> = Arc::new(Mutex::new(None));

        // Trigger 1, before recv #4: both peers are established. Complete both
        // client transports and queue each peer's encrypted hello + a tick.
        let (ws1, out1, kid1) = (mock.clone(), outbound.clone(), agent_kid.clone());
        let (sa, sb) = (slot_a.clone(), slot_b.clone());
        let hellos_ws = TriggerAfterWs::new(mock, 4, move || {
            let out = out1.lock().unwrap().clone();
            init_a.read_msg2(&envs_to(&out, CLIENT_KEY_ID).remove(0));
            init_b.read_msg2(&envs_to(&out, PEER_B).remove(0));
            let mut ta = init_a.into_transport();
            let mut tb = init_b.into_transport();
            let tok_a = injected_clock_token(&kid1, CLIENT_KEY_ID, &["status", "qsy"]);
            let tok_b = injected_clock_token(&kid1, PEER_B, &["status"]);
            ws1.push_inbound(env_from(
                &kid1,
                CLIENT_KEY_ID,
                &client_encrypt(&mut ta, &hello_frame_bytes(&tok_a)),
            ));
            ws1.push_inbound(env_from(
                &kid1,
                PEER_B,
                &client_encrypt(&mut tb, &hello_frame_bytes(&tok_b)),
            ));
            ws1.push_inbound(idle_tick());
            *sa.lock().unwrap() = Some(ta);
            *sb.lock().unwrap() = Some(tb);
        });

        // Trigger 2, before recv #7 (after both hellos were dispatched): the
        // refresh drops A. Then A sends a QSY and a display event goes out.
        let (ws2, out2, kid2) = (hellos_ws.inner.clone(), outbound.clone(), agent_kid.clone());
        let (poll_allow, poll_generation) =
            (ctx.tx_allow_list.clone(), ctx.authz_generation.clone());
        let sa2 = slot_a.clone();
        let envs_to_a_at_revocation = Arc::new(Mutex::new(None));
        let snapshot = envs_to_a_at_revocation.clone();
        let evt_tx2 = evt_tx.clone();
        let ws = TriggerAfterWs::new(hellos_ws, 7, move || {
            poll_allow.write().unwrap().remove(CLIENT_KEY_ID);
            poll_generation.fetch_add(1, Ordering::AcqRel);
            let qsy = serde_json::to_vec(&json!({
                "frame": "command",
                "command": { "cmd": "setFrequency", "vfo": 0, "frequencyHz": 7074000 }
            }))
            .unwrap();
            let ct = client_encrypt(sa2.lock().unwrap().as_mut().unwrap(), &qsy);
            ws2.push_inbound(idle_tick());
            ws2.push_inbound(env_from(&kid2, CLIENT_KEY_ID, &ct));
            ws2.push_inbound(idle_tick());
            evt_tx2
                .send(pancetta_protocol::ServerEvent::TxStatus { active: true })
                .unwrap();
            *snapshot.lock().unwrap() = Some(envs_to(&out2.lock().unwrap(), CLIENT_KEY_ID).len());
        });

        run_one_session(ws, &identity, &mut ctx, &bus, &mut events).await;
        drop(evt_tx);

        let at_revocation = envs_to_a_at_revocation
            .lock()
            .unwrap()
            .expect("trigger 2 fired");
        let out = outbound.lock().unwrap().clone();
        assert_eq!(at_revocation, 2, "setup: A had its msg2 and hello greeting");
        assert_eq!(
            envs_to(&out, CLIENT_KEY_ID).len(),
            at_revocation,
            "A is sent nothing after its revocation"
        );
        assert!(
            hamlib_rx.try_recv().is_err(),
            "A's QSY after revocation reaches no rig"
        );
        assert!(
            !ctx.peers.contains_key(CLIENT_KEY_ID),
            "A's PeerCtx is gone"
        );
        assert!(ctx.revoked_jtis.contains(&format!("hello-{CLIENT_KEY_ID}")));
        assert!(ctx.peers.contains_key(PEER_B), "B stays");

        let envs_b = envs_to(&out, PEER_B);
        assert_eq!(envs_b.len(), 3, "B: msg2, hello greeting, the event");
        let mut tb = slot_b.lock().unwrap().take().unwrap();
        let greeting = client_decrypt_json(&mut tb, &envs_b[1]);
        assert_eq!(greeting["event"]["event"], "controlState");
        let event = client_decrypt_json(&mut tb, &envs_b[2]);
        assert_eq!(event["event"]["event"], "txStatus");
        assert_eq!(event["event"]["active"], true);
    }

    // ========================================================================
    // PAN-97: a lapsed arm needs a fresh signed arm — real session loop
    // ========================================================================

    /// Builds one control-frame plaintext for a scripted [`Pan97Client`].
    type Pan97Frame = Box<dyn FnOnce(&Pan97Client) -> Vec<u8> + Send>;

    /// One scripted receive for [`Pan97Ws`].
    enum Pan97Step {
        /// Deliver a pre-built relay frame (clock unchanged).
        Frame(String),
        /// Set the injected clock to `ms`, then report a quiet receive tick.
        QuietAt(i64),
        /// Set the clock to `ms`, then deliver the plaintext built by the
        /// closure, encrypted under client `idx`'s live Noise transport.
        SendAt(i64, usize, Pan97Frame),
        /// Run a check between ticks (clock unchanged), then report quiet.
        Check(Box<dyn FnOnce() + Send>),
        /// Report quiet; the agent's next outbound send then "blocks" on
        /// relay backpressure until the clock reads `ms`.
        StallNextSendUntil(i64),
    }

    /// The client side of one Noise peer in a scripted `run_one_session`:
    /// completes the handshake from the agent's msg2, encrypts control frames
    /// and decrypts every event the agent sends back, in wire order.
    struct Pan97Client {
        kid: String,
        agent_kid: String,
        initiator: Option<TestInitiator>,
        transport: Option<snow::TransportState>,
        session_id: String,
        envs_seen: usize,
        events: Vec<Value>,
    }

    impl Pan97Client {
        fn new(kid: &str, agent_kid: &str, agent_pub: &[u8]) -> Self {
            Self {
                kid: kid.to_string(),
                agent_kid: agent_kid.to_string(),
                initiator: Some(initiator_for(&device_for(kid), agent_pub)),
                transport: None,
                session_id: String::new(),
                envs_seen: 0,
                events: Vec::new(),
            }
        }

        /// The relay env carrying this client's handshake msg1 (with the
        /// PAN-95 client identity proof from its enrolled device).
        fn msg1_env(&mut self) -> String {
            let proof = proof_for(&device_for(&self.kid));
            let msg1 = self.initiator.as_mut().unwrap().write_msg1(&proof);
            RelayFrame::Env {
                dst: self.agent_kid.clone(),
                payload: b64url(&msg1),
                src: Some(self.kid.clone()),
            }
            .to_json()
            .unwrap()
        }

        /// Absorb new agent output: the first env is msg2 (completes the
        /// handshake); every later one is an encrypted event.
        fn sync(&mut self, outbound: &[String]) {
            let envs: Vec<Vec<u8>> = outbound
                .iter()
                .filter_map(|s| match parse_frame(s).unwrap() {
                    RelayFrame::Env { dst, payload, .. } if dst == self.kid => Some(
                        base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(&payload)
                            .unwrap(),
                    ),
                    _ => None,
                })
                .collect();
            for env in envs.iter().skip(self.envs_seen) {
                if let Some(mut init) = self.initiator.take() {
                    init.read_msg2(env);
                    self.session_id = b64url(&init.handshake_hash());
                    self.transport = Some(init.into_transport());
                } else {
                    let transport = self.transport.as_mut().unwrap();
                    let mut buf = vec![0u8; env.len().max(1)];
                    let n = transport.read_message(env, &mut buf).unwrap();
                    buf.truncate(n);
                    self.events.push(serde_json::from_slice(&buf).unwrap());
                }
            }
            self.envs_seen = envs.len();
        }

        fn encrypt_env(&mut self, plaintext: &[u8]) -> String {
            let transport = self
                .transport
                .as_mut()
                .expect("handshake must complete before sending");
            let mut ct = vec![0u8; plaintext.len() + 16];
            let n = transport.write_message(plaintext, &mut ct).unwrap();
            ct.truncate(n);
            RelayFrame::Env {
                dst: self.agent_kid.clone(),
                payload: b64url(&ct),
                src: Some(self.kid.clone()),
            }
            .to_json()
            .unwrap()
        }

        /// `(controlHeldByMe, transmitArmed)` of every `controlState` received.
        fn control_states(&self) -> Vec<(bool, bool)> {
            self.events
                .iter()
                .filter(|e| e["event"]["event"] == "controlState")
                .map(|e| {
                    (
                        e["event"]["controlHeldByMe"].as_bool().unwrap(),
                        e["event"]["transmitArmed"].as_bool().unwrap(),
                    )
                })
                .collect()
        }
    }

    /// A relay transport whose every receive comes from a script, advancing
    /// the session's injected clock as it goes. An exhausted script closes.
    struct Pan97Ws {
        outbound: Arc<Mutex<Vec<String>>>,
        steps: std::collections::VecDeque<Pan97Step>,
        clients: Arc<Mutex<Vec<Pan97Client>>>,
        clock_ms: Arc<std::sync::atomic::AtomicI64>,
        stall_next_send: Option<i64>,
    }

    impl WsConn for Pan97Ws {
        fn send_text(&mut self, s: String) -> Result<(), RelayError> {
            if let Some(ms) = self.stall_next_send.take() {
                self.clock_ms.store(ms, Ordering::SeqCst);
            }
            self.outbound.lock().unwrap().push(s);
            Ok(())
        }
        fn recv_text(&mut self) -> Result<Option<String>, RelayError> {
            match self.recv_text_within(Duration::ZERO)? {
                pancetta_agent::relay::RecvOutcome::Frame(t) => Ok(Some(t)),
                _ => Ok(None),
            }
        }
        fn recv_text_within(
            &mut self,
            _timeout: Duration,
        ) -> Result<pancetta_agent::relay::RecvOutcome, RelayError> {
            use pancetta_agent::relay::RecvOutcome;
            let out = self.outbound.lock().unwrap().clone();
            for c in self.clients.lock().unwrap().iter_mut() {
                c.sync(&out);
            }
            Ok(match self.steps.pop_front() {
                None => RecvOutcome::Closed,
                Some(Pan97Step::Frame(t)) => RecvOutcome::Frame(t),
                Some(Pan97Step::QuietAt(ms)) => {
                    self.clock_ms.store(ms, Ordering::SeqCst);
                    RecvOutcome::Quiet
                }
                Some(Pan97Step::SendAt(ms, idx, build)) => {
                    self.clock_ms.store(ms, Ordering::SeqCst);
                    let mut clients = self.clients.lock().unwrap();
                    let plaintext = build(&clients[idx]);
                    RecvOutcome::Frame(clients[idx].encrypt_env(&plaintext))
                }
                Some(Pan97Step::Check(f)) => {
                    f();
                    RecvOutcome::Quiet
                }
                Some(Pan97Step::StallNextSendUntil(ms)) => {
                    self.stall_next_send = Some(ms);
                    RecvOutcome::Quiet
                }
            })
        }
    }

    /// A scripted PAN-97 session: CLIENT_KEY_ID (TX-arm eligible) plus any
    /// `extra` listener peers, consent ON, clock at `NOW`.
    struct Pan97Session {
        identity: AgentIdentity,
        ctx: ArmContext,
        clients: Vec<Pan97Client>,
        clock_ms: Arc<std::sync::atomic::AtomicI64>,
    }

    impl Pan97Session {
        fn new(extra: &[&str]) -> Self {
            let identity = AgentIdentity::generate();
            let agent_kid = identity.key_id();
            let agent_pub = identity.agreement_public_raw();
            let mut ctx = ctx_with_agent(&agent_kid, true, true);
            for peer in extra {
                ctx.tx_allow_list.write().unwrap().insert(peer.to_string());
            }
            with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
            let clock_ms = Arc::new(std::sync::atomic::AtomicI64::new(NOW));
            ctx.clock = manual_clock(&clock_ms);
            let clients = std::iter::once(CLIENT_KEY_ID)
                .chain(extra.iter().copied())
                .map(|kid| Pan97Client::new(kid, &agent_kid, &agent_pub))
                .collect();
            Self {
                identity,
                ctx,
                clients,
                clock_ms,
            }
        }

        /// Run the real `run_one_session` over relay hello + every client's
        /// msg1, then `script`. Returns the clients with all output decrypted.
        async fn run(
            mut self,
            script: Vec<Pan97Step>,
            events: &mut Option<tokio::sync::broadcast::Receiver<pancetta_protocol::ServerEvent>>,
        ) -> (ArmContext, Vec<Pan97Client>) {
            let hello = RelayFrame::Hello {
                challenge: b64url(&[9u8; 32]),
            }
            .to_json()
            .unwrap();
            let mut steps: std::collections::VecDeque<Pan97Step> =
                std::iter::once(Pan97Step::Frame(hello)).collect();
            for (idx, c) in self.clients.iter_mut().enumerate() {
                steps.push_back(Pan97Step::Frame(c.msg1_env()));
                // PAN-95: an established peer is sent nothing until it holds
                // a verified hello capability, so every scripted client
                // hellos first (on the injected NOW timeline) — the arm and
                // disarm broadcasts under test must actually reach them.
                steps.push_back(Pan97Step::SendAt(NOW, idx, pan97_hello()));
            }
            steps.extend(script);
            let outbound = Arc::new(Mutex::new(Vec::new()));
            let clients = Arc::new(Mutex::new(std::mem::take(&mut self.clients)));
            let ws = Pan97Ws {
                outbound: outbound.clone(),
                steps,
                clients: clients.clone(),
                clock_ms: self.clock_ms.clone(),
                stall_next_send: None,
            };
            let bus = MessageBus::new(64).unwrap();
            run_one_session(ws, &self.identity, &mut self.ctx, &bus, events).await;
            let out = outbound.lock().unwrap().clone();
            let mut clients = std::mem::take(&mut *clients.lock().unwrap());
            for c in clients.iter_mut() {
                c.sync(&out);
            }
            (self.ctx, clients)
        }
    }

    /// A `hello` frame carrying a NOW-timeline capability token for the
    /// sending client (PAN-95: a peer receives nothing until this verifies).
    fn pan97_hello() -> Pan97Frame {
        Box::new(|c: &Pan97Client| {
            hello_frame_bytes(&injected_clock_token(&c.agent_kid, &c.kid, &["status"]))
        })
    }

    fn pan97_arm(jti: &'static str, armed_until: i64) -> Pan97Frame {
        Box::new(move |c: &Pan97Client| {
            serde_json::to_vec(&tx_arm_frame(&c.agent_kid, &c.session_id, jti, armed_until))
                .unwrap()
        })
    }

    fn pan97_heartbeat(jti: &'static str, seq: u64) -> Pan97Frame {
        Box::new(move |_: &Pan97Client| {
            serde_json::to_vec(&json!({"type": "txHeartbeat", "armJti": jti, "seq": seq})).unwrap()
        })
    }

    /// A valid control frame that touches neither the arm nor control. An
    /// unknown frame tag maps to `Unsupported` (a logged no-op); a legacy
    /// no-token `hello` is NOT benign anymore — since PAN-96 it clears the
    /// sender's capability and disarms an authed controller.
    fn pan97_benign() -> Pan97Frame {
        Box::new(|_: &Pan97Client| {
            serde_json::to_vec(&json!({
                "frame": "statusPing",
                "statusPing": { "protocolVersion": 1 }
            }))
            .unwrap()
        })
    }

    fn pan97_check(f: impl FnOnce() + Send + 'static) -> Pan97Step {
        Pan97Step::Check(Box::new(f))
    }

    /// How many audit records contain `needle`.
    fn audit_count(ctx: &ArmContext, needle: &str) -> usize {
        std::fs::read_to_string(ctx.audit.path())
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(needle))
            .count()
    }

    /// The ticket's scenario on the real loop: an established, armed
    /// controller goes silent. With no heartbeat and no TX request, the
    /// session loop's own sweep expires the arm at the deadline, audits it
    /// once and tells every peer `transmitArmed: false`, keeping the peers
    /// and the controller slot.
    #[tokio::test]
    async fn silent_controller_is_disarmed_by_the_session_loop_without_any_heartbeat() {
        let sess = Pan97Session::new(&[PEER_B]);
        let (arm_a, arm_b) = (sess.ctx.arm.clone(), sess.ctx.arm.clone());
        let script = vec![
            Pan97Step::SendAt(NOW, 0, pan97_arm("arm-jti-1", NOW + 300_000)),
            pan97_check(move || assert!(arm_a.lock().unwrap().is_armed(), "armed")),
            Pan97Step::QuietAt(NOW + HEARTBEAT_TIMEOUT_MS - 1),
            Pan97Step::QuietAt(NOW + HEARTBEAT_TIMEOUT_MS),
            pan97_check(move || {
                assert!(
                    !arm_b.lock().unwrap().is_armed(),
                    "the sweep must expire the arm"
                )
            }),
        ];
        let (ctx, clients) = sess.run(script, &mut None).await;

        assert!(!ctx.arm.lock().unwrap().is_armed());
        assert_eq!(audit_count(&ctx, "auto-disarm: heartbeat-lost"), 1);
        assert!(ctx.peers.contains_key(CLIENT_KEY_ID) && ctx.peers.contains_key(PEER_B));
        assert_eq!(ctx.controller.as_deref(), Some(CLIENT_KEY_ID));
        assert_eq!(clients[0].control_states().last(), Some(&(true, false)));
        assert_eq!(clients[1].control_states().last(), Some(&(false, false)));
        assert!(
            clients[0].control_states().contains(&(true, true)),
            "the controller saw the arm go live first"
        );
    }

    /// Continuous valid (non-heartbeat) traffic from both peers cannot starve
    /// the sweep: the frame that arrives at the deadline is handled only after
    /// the arm has expired.
    #[tokio::test]
    async fn valid_traffic_cannot_skip_the_session_loop_expiry_sweep() {
        let sess = Pan97Session::new(&[PEER_B]);
        let arm = sess.ctx.arm.clone();
        let script = vec![
            Pan97Step::SendAt(NOW, 0, pan97_arm("arm-jti-1", NOW + 300_000)),
            Pan97Step::SendAt(NOW + 10_000, 0, pan97_benign()),
            Pan97Step::SendAt(NOW + 20_000, 1, pan97_benign()),
            Pan97Step::SendAt(NOW + HEARTBEAT_TIMEOUT_MS, 1, pan97_benign()),
            pan97_check(move || assert!(!arm.lock().unwrap().is_armed())),
        ];
        let (ctx, clients) = sess.run(script, &mut None).await;

        assert_eq!(audit_count(&ctx, "auto-disarm: heartbeat-lost"), 1);
        assert_eq!(clients[0].control_states().last(), Some(&(true, false)));
        assert_eq!(clients[1].control_states().last(), Some(&(false, false)));
    }

    /// A malformed decrypted frame takes the loop's early `continue`; it
    /// must not bypass the sweep either.
    #[tokio::test]
    async fn malformed_traffic_cannot_skip_the_session_loop_expiry_sweep() {
        let sess = Pan97Session::new(&[PEER_B]);
        let arm = sess.ctx.arm.clone();
        let script = vec![
            Pan97Step::SendAt(NOW, 0, pan97_arm("arm-jti-1", NOW + 300_000)),
            Pan97Step::SendAt(
                NOW + HEARTBEAT_TIMEOUT_MS,
                1,
                Box::new(|_: &Pan97Client| b"not valid json{{{".to_vec()),
            ),
            pan97_check(move || assert!(!arm.lock().unwrap().is_armed())),
        ];
        let (ctx, clients) = sess.run(script, &mut None).await;

        assert_eq!(audit_count(&ctx, "auto-disarm: heartbeat-lost"), 1);
        assert!(
            ctx.peers.contains_key(PEER_B),
            "malformed frame keeps the peer"
        );
        assert_eq!(clients[0].control_states().last(), Some(&(true, false)));
        assert_eq!(clients[1].control_states().last(), Some(&(false, false)));
    }

    /// `(controlHeldByMe, transmitArmed)` per recipient of `sends`.
    fn pan97_states(sends: &[PeerSend]) -> Vec<(String, bool, bool)> {
        let mut states: Vec<_> = sends
            .iter()
            .filter_map(|s| {
                let SendTarget::One(peer) = &s.to;
                match &s.frame {
                    pancetta_protocol::ServerFrame::Event {
                        event:
                            pancetta_protocol::ServerEvent::ControlState {
                                control_held_by_me,
                                transmit_armed,
                            },
                    } => Some((peer.clone(), *control_held_by_me, *transmit_armed)),
                    _ => None,
                }
            })
            .collect();
        states.sort();
        states
    }

    /// The clock advances between the loop's pre-dispatch sample and the arm
    /// mutation (which re-samples under the lock): the successful-arm
    /// `controlState` must agree with the live gate, not report the new arm
    /// as already expired against the older sample.
    #[tokio::test]
    async fn arm_response_reflects_arm_accepted_after_dispatch_sample() {
        let bus = MessageBus::new(64).unwrap();
        let mut ctx = ctx_with(true, true);
        ctx.controller = Some(CLIENT_KEY_ID.to_string());
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        ctx.clock = fixed_clock(NOW + 1);
        let d = dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted((ctx.clock)(), ConsentChannel::StationAgent));
        assert_eq!(
            pan97_states(&d.sends),
            vec![(CLIENT_KEY_ID.to_string(), true, true)]
        );
    }

    /// The sweep re-samples under the arm lock: a deadline that passed after
    /// the loop's sample still expires the arm on this iteration.
    #[tokio::test]
    async fn sweep_uses_clock_sampled_under_arm_lock() {
        let bus = MessageBus::new(64).unwrap();
        let mut ctx = ctx_with(true, true);
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        ctx.clock = fixed_clock(NOW + HEARTBEAT_TIMEOUT_MS);
        assert_eq!(
            pan97_states(&sweep_arm_expiry(&ctx, at(NOW))),
            vec![(CLIENT_KEY_ID.to_string(), true, false)]
        );
        assert!(!ctx.arm.lock().unwrap().is_armed());
    }

    /// A late heartbeat reaching dispatch before any sweep expires the arm
    /// itself and broadcasts the transition; once expired (by it or by the
    /// sweep), further heartbeats change nothing and audit nothing.
    #[tokio::test]
    async fn late_heartbeat_dispatch_expires_and_broadcasts_exactly_once() {
        let bus = MessageBus::new(64).unwrap();
        let heartbeat = |seq| ControlAction::Heartbeat {
            arm_jti: "arm-jti-1".to_string(),
            seq,
        };
        // Sorted by keyId (pan97_states sorts): the real PEER_B keyId sorts
        // before CLIENT_KEY_ID.
        let expected = vec![
            (PEER_B.to_string(), false, false),
            (CLIENT_KEY_ID.to_string(), true, false),
        ];

        // Heartbeat first, no housekeeping. PEER_B sends it, so it must be
        // admitted (PAN-96 refuses every action from a non-admitted peer,
        // heartbeats included — refusing can only shorten an arm).
        let mut ctx = ctx_with(true, true);
        ctx.tx_allow_list
            .write()
            .unwrap()
            .insert(PEER_B.to_string());
        ctx.peers
            .insert(PEER_B.to_string(), PeerCtx::unauthorized());
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        assert!(ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(at(NOW), ConsentChannel::StationAgent));
        let late = at(NOW + HEARTBEAT_TIMEOUT_MS + 1);
        let d = dispatch_action(heartbeat(1), PEER_B, &mut ctx, &bus, SESSION_ID, late).await;
        assert_eq!(pan97_states(&d.sends), expected);
        assert!(!ctx.arm.lock().unwrap().is_armed());
        assert!(!ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(late, ConsentChannel::StationAgent));
        let d = dispatch_action(
            heartbeat(2),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            late,
        )
        .await;
        assert!(d.sends.is_empty(), "no duplicate transition");
        assert_eq!(audit_count(&ctx, "auto-disarm: heartbeat-lost"), 1);
        assert_eq!(ctx.controller.as_deref(), Some(CLIENT_KEY_ID));

        // Housekeeping first, then the late heartbeat.
        let mut ctx = ctx_with(true, true);
        ctx.peers
            .insert(PEER_B.to_string(), PeerCtx::unauthorized());
        with_channel_consent(&ctx, ConsentChannel::StationAgent, true);
        dispatch_action(
            arm_action("arm-jti-1"),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            at(NOW),
        )
        .await;
        let deadline = at(NOW + HEARTBEAT_TIMEOUT_MS);
        assert_eq!(pan97_states(&sweep_arm_expiry(&ctx, deadline)), expected);
        assert!(
            sweep_arm_expiry(&ctx, deadline).is_empty(),
            "sweep is idempotent"
        );
        let d = dispatch_action(
            heartbeat(1),
            CLIENT_KEY_ID,
            &mut ctx,
            &bus,
            SESSION_ID,
            deadline,
        )
        .await;
        assert!(d.sends.is_empty());
        assert!(!ctx
            .arm
            .lock()
            .unwrap()
            .tx_permitted(deadline, ConsentChannel::StationAgent));
        assert_eq!(audit_count(&ctx, "auto-disarm: heartbeat-lost"), 1);
    }

    /// Recovery on the same session: the consumed grant replays as a
    /// rejection, an old-arm heartbeat does nothing, and only a newly signed
    /// grant with a fresh jti restores TX — whose own seq 1 heartbeat is
    /// accepted while the old jti still cannot slide its window.
    #[tokio::test]
    async fn only_a_fresh_signed_arm_restores_tx_after_a_lapse() {
        let sess = Pan97Session::new(&[]);
        let (a1, a2, a3) = (
            sess.ctx.arm.clone(),
            sess.ctx.arm.clone(),
            sess.ctx.arm.clone(),
        );
        let script = vec![
            Pan97Step::SendAt(NOW, 0, pan97_arm("arm-jti-1", NOW + 300_000)),
            Pan97Step::QuietAt(NOW + HEARTBEAT_TIMEOUT_MS),
            // The consumed grant again: replay-rejected, still disarmed.
            Pan97Step::SendAt(NOW + 31_000, 0, pan97_arm("arm-jti-1", NOW + 300_000)),
            Pan97Step::SendAt(NOW + 32_000, 0, pan97_heartbeat("arm-jti-1", 7)),
            pan97_check(move || assert!(!a1.lock().unwrap().is_armed(), "no resurrection")),
            // A freshly signed grant.
            Pan97Step::SendAt(NOW + 33_000, 0, pan97_arm("arm-jti-2", NOW + 300_000)),
            pan97_check(move || {
                let st = a2.lock().unwrap();
                assert_eq!(st.current_arm_jti(), Some("arm-jti-2"));
                assert!(st.tx_permitted(at(NOW + 33_000), ConsentChannel::StationAgent));
            }),
            Pan97Step::SendAt(NOW + 40_000, 0, pan97_heartbeat("arm-jti-2", 1)),
            Pan97Step::SendAt(NOW + 50_000, 0, pan97_heartbeat("arm-jti-1", 9)),
            pan97_check(move || {
                let st = a3.lock().unwrap();
                assert!(st.tx_permitted(
                    at(NOW + 40_000 + HEARTBEAT_TIMEOUT_MS - 1),
                    ConsentChannel::StationAgent
                ));
                assert!(
                    !st.tx_permitted(
                        at(NOW + 40_000 + HEARTBEAT_TIMEOUT_MS),
                        ConsentChannel::StationAgent
                    ),
                    "the old jti must not slide the new arm's window"
                );
            }),
        ];
        let (ctx, clients) = sess.run(script, &mut None).await;

        assert_eq!(audit_count(&ctx, "auto-disarm: heartbeat-lost"), 1);
        assert_eq!(
            audit_count(&ctx, "arm rejected: arm grant: replayed jti"),
            1
        );
        assert_eq!(audit_count(&ctx, "heartbeat rejected: arm_jti mismatch"), 1);
        assert!(ctx.seen_jtis.contains("arm-jti-1") && ctx.seen_jtis.contains("arm-jti-2"));
        let states = clients[0].control_states();
        assert_eq!(
            states.last(),
            Some(&(true, true)),
            "fresh arm is live again"
        );
        assert!(states.contains(&(true, false)), "the lapse was announced");
    }

    /// Timely heartbeats do not outlive the signed `armedUntil`: the sweep
    /// expires the arm for TTL exactly once, and a racing revocation of the
    /// already-consumed arm adds no second disarm record.
    #[tokio::test]
    async fn session_loop_sweep_enforces_signed_expiry_despite_timely_heartbeats() {
        let sess = Pan97Session::new(&[]);
        let (a1, a2) = (sess.ctx.arm.clone(), sess.ctx.arm.clone());
        let revocation_pending = sess.ctx.revocation_pending.clone();
        let script = vec![
            Pan97Step::SendAt(NOW, 0, pan97_arm("arm-jti-1", NOW + 60_000)),
            Pan97Step::SendAt(NOW + 20_000, 0, pan97_heartbeat("arm-jti-1", 1)),
            Pan97Step::SendAt(NOW + 40_000, 0, pan97_heartbeat("arm-jti-1", 2)),
            Pan97Step::QuietAt(NOW + 59_999),
            pan97_check(move || assert!(a1.lock().unwrap().is_armed())),
            Pan97Step::QuietAt(NOW + 60_000),
            pan97_check(move || {
                let mut st = a2.lock().unwrap();
                assert!(!st.is_armed());
                assert!(st
                    .disarm_with_reason(DisarmReason::AuthorizationRevoked, NOW + 60_000)
                    .is_empty());
                revocation_pending.store(true, Ordering::Release);
            }),
            Pan97Step::QuietAt(NOW + 61_000),
        ];
        let (ctx, clients) = sess.run(script, &mut None).await;

        assert_eq!(audit_count(&ctx, "auto-disarm: ttl-expired"), 1);
        assert_eq!(audit_count(&ctx, "heartbeat-lost"), 0);
        assert_eq!(audit_count(&ctx, "authorization-revoked"), 0);
        assert_eq!(clients[0].control_states().last(), Some(&(true, false)));
    }

    /// The display drain hands control back to the loop after at most
    /// `READ_STREAM_DRAIN_MAX` items, so a flooded read stream cannot hold
    /// off the sweep.
    #[tokio::test]
    async fn bounded_display_drain_returns_to_the_expiry_sweep() {
        let sess = Pan97Session::new(&[]);
        sess.ctx.arm.lock().unwrap().arm(
            pancetta_agent::arm::VerifiedArmGrant {
                operator_callsign: OPERATOR.to_string(),
                ttl_ms: 300_000,
                armed_until_ms: NOW + 300_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: "arm-jti-1".to_string(),
                client_key_id: CLIENT_KEY_ID.to_string(),
            },
            at(NOW),
        );
        let (tx, rx) = tokio::sync::broadcast::channel(512);
        let queued = 3 * READ_STREAM_DRAIN_MAX + 10;
        for _ in 0..queued {
            tx.send(pancetta_protocol::ServerEvent::ControlState {
                control_held_by_me: false,
                transmit_armed: false,
            })
            .unwrap();
        }
        let arm = sess.ctx.arm.clone();
        let script = vec![
            Pan97Step::QuietAt(NOW + HEARTBEAT_TIMEOUT_MS),
            pan97_check(move || assert!(!arm.lock().unwrap().is_armed())),
        ];
        let mut events = Some(rx);
        let (ctx, _) = sess.run(script, &mut events).await;

        assert_eq!(audit_count(&ctx, "auto-disarm: heartbeat-lost"), 1);
        // The client's hello dispatch drains one batch (PAN-95: a peer
        // receives nothing until it holds a verified hello), and each of the
        // two quiet ticks drains exactly one more bounded batch.
        assert_eq!(
            events.as_ref().unwrap().len(),
            queued - 3 * READ_STREAM_DRAIN_MAX,
            "each loop iteration drains at most READ_STREAM_DRAIN_MAX items"
        );
    }

    /// The sweep's expiry broadcast can stall on relay backpressure. The frame
    /// received with it is authorized at a fresh instant afterwards, so a
    /// capability that lapsed during the stall cannot establish a new arm.
    #[tokio::test]
    async fn frame_after_a_stalled_expiry_broadcast_is_authorized_at_a_fresh_instant() {
        // The hello and arm capability tokens both expire at NOW + 600 s.
        let cap_exp = NOW + 600_000;
        let sess = Pan97Session::new(&[]);
        let script = vec![
            Pan97Step::SendAt(NOW + 560_000, 0, pan97_arm("arm-jti-1", NOW + 900_000)),
            Pan97Step::StallNextSendUntil(cap_exp),
            // arm-jti-1's heartbeat deadline: the sweep expires it, and its
            // controlState send stalls until the capability has expired.
            Pan97Step::SendAt(NOW + 590_000, 0, pan97_arm("arm-jti-2", NOW + 900_000)),
        ];
        let (ctx, _) = sess.run(script, &mut None).await;

        assert_eq!(audit_count(&ctx, "auto-disarm: heartbeat-lost"), 1);
        assert_eq!(
            audit_count(&ctx, "armed ttl_ms="),
            1,
            "arm-jti-2 must not arm on a capability that expired during the stall"
        );
        assert_eq!(audit_count(&ctx, "arm rejected: capability"), 1);
    }

    /// Housekeeping likewise checks capability expiry after its own sends: a
    /// controller whose capability lapsed while a broadcast stalled is
    /// disarmed on that same tick.
    #[tokio::test]
    async fn housekeeping_checks_capability_expiry_after_a_stalled_broadcast() {
        let cap_exp = NOW + 600_000;
        let sess = Pan97Session::new(&[]);
        let arm = sess.ctx.arm.clone();
        let revocation_pending = sess.ctx.revocation_pending.clone();
        let script = vec![
            Pan97Step::SendAt(NOW + 580_000, 0, pan97_arm("arm-jti-1", NOW + 900_000)),
            Pan97Step::QuietAt(NOW + 590_000),
            Pan97Step::StallNextSendUntil(cap_exp),
            // Any housekeeping broadcast will do; this one stalls until the
            // controller's capability has expired.
            pan97_check(move || revocation_pending.store(true, Ordering::Release)),
            // Runs before the next tick's sweep and housekeeping.
            pan97_check(move || {
                assert!(
                    !arm.lock().unwrap().is_armed(),
                    "the expired controller's arm must drop on the stalled tick"
                )
            }),
        ];
        let (ctx, _) = sess.run(script, &mut None).await;

        assert_eq!(audit_count(&ctx, "armed ttl_ms="), 1);
        assert!(ctx.controller.is_none());
    }
}
