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
//! - **Controller model — one controller at a time, scoped grab:** at most one
//!   established peer holds `controller` (arm/QSY/split/TX-initiation)
//!   at a time; every other connected peer is a live read-only viewer.
//!   `takeControl` succeeds for any admitted peer holding a currently-valid
//!   `qsy`-or-higher hello capability (single-operator assumption); from a
//!   status-only or unauthorized peer it is refused (`warn!`, no frame) and
//!   changes nothing (PAN-253). If an authorized takeover displaces a
//!   different controller whose arm is live,
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

/// Live station state refreshed across QSO-component restarts.
struct StationActivity {
    ptt_active: Arc<std::sync::atomic::AtomicBool>,
    qso_manager: tokio::sync::watch::Receiver<Option<pancetta_qso::QsoManager>>,
}

#[cfg(test)]
impl StationActivity {
    fn idle() -> Self {
        Self {
            ptt_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            qso_manager: tokio::sync::watch::channel(None).1,
        }
    }
}

/// Everything the dispatch loop needs to act on a decrypted control action:
/// the shared arm, the capability verifier, the client's device verifying key,
/// the station-local TX-allow-list, the session replay set, and the audit log.
///
/// Held on the blocking session task; the arm is the coordinator's shared
/// `remote_tx_arm`, so arming here is visible to the TX worker immediately.
struct ArmContext {
    station_activity: StationActivity,
    arm: Arc<Mutex<ArmState>>,
    /// The coordinator's shared split-TX dial (0 = simplex) — the source the
    /// TX band-plan gate (`tx::tx_rf_for_gate`) and the QSO RF stamp read. A
    /// remote `SetSplit` writes it via [`super::forward_split_change`] (the
    /// same helper the local TUI path uses) once Hamlib's channel accepts the
    /// change, so a remote split is gated exactly like a local one (PR #446
    /// Codex review) and a dropped one never loosens the gate (PAN-209).
    split_tx_frequency_hz: Arc<std::sync::atomic::AtomicU64>,
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
    /// PAN-147: owns this context's audit temp path so it is cleaned (with
    /// its rotation archives) when the context drops, and pre-cleaned before
    /// use against a PID-reusing earlier run's leftover. Test-only; `None`
    /// in production and compiled away outside `#[cfg(test)]`.
    #[cfg(test)]
    _audit_tmp: Option<crate::coordinator::audit_test_util::TempPath>,
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
    /// PAN-137: changes to Some(peer) owe cleanup of other clients' QSOs.
    controller: Option<String>,
    /// PAN-137: newest controller cleanup not yet confirmed enqueued. Kept
    /// across session teardown because the displacement already happened.
    qso_cleanup_owed: Option<OwedQsoCleanup>,
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
    /// task (a SEPARATE tokio task, like `revocation_pending`). In
    /// static-config mode its only bumper is a saved config edit that removes
    /// `tx_allow_list` entries (PAN-103, [`revoke_static_allow_list_entries`]).
    authz_generation: Arc<std::sync::atomic::AtomicU64>,
    /// The generation THIS session last reconciled to. When it lags
    /// `authz_generation`, [`reconcile_authorizations`] applies the refresh to
    /// the live session on the next tick.
    authz_generation_seen: u64,
    /// The arm-timing clock (wall + monotonic). Production uses
    /// [`ArmTime::now`]; session tests inject a manually advanced timeline.
    clock: ArmClock,
    /// The component's shutdown flag. `run_one_session` checks it every tick
    /// so a requested shutdown ends the session (and disarms) without
    /// waiting for a socket event (PAN-102).
    shutdown: Arc<std::sync::atomic::AtomicBool>,
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
    let (armer_evicted, effects) = {
        let mut st = match ctx.arm.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let armer_evicted = st
            .armed_client_key_id()
            .is_some_and(|k| evicted.iter().any(|p| p == k));
        let mut effects = Vec::new();
        if controller_evicted || armer_evicted {
            effects = st.disarm_with_reason(DisarmReason::AuthorizationRevoked, now.unix_ms);
            if !effects.is_empty() {
                warn!(target: "agent.tx", "controller evicted with a live arm — disarming");
            }
        }
        (armer_evicted, effects)
    };
    // PAN-104: audit I/O only after the arm lock is released.
    apply_arm_effects(&ctx.audit, &effects);
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
            ArmEffect::Audit(ev) => audit.record(ev),
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
/// path -- a session can still be mid-flight when shutdown is requested
/// (`run_one_session` checks the flag once per tick since PAN-102, but a tick
/// blocked in a dispatch or send can outlast the grace period). `shutdown.rs` gives every named task a 1s grace period, then
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
///
/// PAN-103: `stop_requested` is this component's own "deliberately stopped"
/// flag, set by the coordinator right before it aborts the session because a
/// saved config edit disabled the station agent. It narrows the same
/// non-panicking fallback as `shutdown` does, so that deliberate stop is
/// audited as `OperatorDisarm`, never `ComponentCrash`.
struct RemoteTxDisarmGuard {
    arm: Arc<Mutex<ArmState>>,
    audit: AuditLog,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    stop_requested: Arc<std::sync::atomic::AtomicBool>,
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
        let non_panicking_shutdown_cancellation = !std::thread::panicking()
            && (self.shutdown.load(Ordering::Acquire)
                || self.stop_requested.load(Ordering::Acquire));
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

/// PAN-137: cleanup remains owed until the QSO inbox accepts it.
struct OwedQsoCleanup {
    controller: String,
    failed_attempts: u32,
}

/// Record the newest displacement; an undelivered older cleanup is obsolete.
fn owe_qso_cleanup(ctx: &mut ArmContext, peer: &str) {
    info!(target: "agent.control", peer = %peer, "controller changed; ending QSOs bound to other remote clients");
    ctx.qso_cleanup_owed = Some(OwedQsoCleanup {
        controller: peer.to_string(),
        failed_attempts: 0,
    });
}

/// Confirm enqueue on the QSO inbox, keeping failed deliveries owed for the
/// next session tick. This task is the only sender; FIFO delivery and refusing
/// intents while cleanup remains owed prevent an intent overtaking cleanup.
async fn flush_qso_cleanup(ctx: &mut ArmContext, bus: &MessageBus) {
    let Some(controller) = ctx.qso_cleanup_owed.as_ref().map(|o| o.controller.clone()) else {
        return;
    };
    let msg = ComponentMessage::new(
        ComponentId::StationAgent,
        ComponentId::Qso,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::RemoteControllerChanged {
            controller_key_id: controller,
        }),
        std::time::Instant::now(),
    );
    match bus.send_message_checked(msg).await {
        Ok(true) => ctx.qso_cleanup_owed = None,
        Ok(false) | Err(_) => {
            if let Some(owed) = ctx.qso_cleanup_owed.as_mut() {
                owed.failed_attempts = owed.failed_attempts.saturating_add(1);
                if owed.failed_attempts == 1 {
                    warn!(target: "agent.control", "controller-change QSO cleanup not delivered; retrying each tick; displaced clients' QSOs stay active until delivery");
                } else {
                    debug!(target: "agent.control", attempts = owed.failed_attempts, "controller-change QSO cleanup still not delivered");
                }
            }
        }
    }
}

/// The outcome of the one-controller-at-a-time gate for a control-mutating
/// action (`Arm`/`Qsy`/`SetSplit`/`TxRequest`/`StopCq`). Every caller checks the
/// peer's hello capability first (PAN-283: `qsy` for all but `StopCq`, which
/// needs any live scope), so the gate itself never looks at the capability.
enum ControllerGate {
    /// The peer may proceed. Carries any `controlState` frames from an implicit
    /// grab (empty when the peer was already the controller).
    Proceed(Vec<PeerSend>),
    /// The peer is NOT the controller — the action is refused; carries the
    /// `Error` frame to send back to just that peer.
    Refused(Vec<PeerSend>),
}

/// Apply rules 3 (implicit grab) + 4 (exclusivity) for a control-mutating
/// action from `peer`. Callers reach it only after the peer's hello capability
/// check (PAN-283: [`refuse_without_control_capability`] for
/// `Arm`/`Qsy`/`SetSplit`/`TxRequest`; any live scope for `StopCq`), so an
/// implicit grab is never made by a peer without a currently-valid hello:
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
            // zero behavior change. PAN-137 also cleans up other clients' QSOs.
            // The caller has already checked the hello capability (PAN-283).
            ctx.controller = Some(peer.to_string());
            owe_qso_cleanup(ctx, peer);
            debug!(target: "agent.control", peer = %peer, "implicit control grab (no controller set)");
            ControllerGate::Proceed(control_state_sends(ctx, now, None))
        }
        Some(p) if p == peer => ControllerGate::Proceed(Vec::new()),
        Some(_) => {
            // Rule 4 — exclusivity.
            warn!(target: "agent.control", peer = %peer, "control action refused — another client holds control");
            ctx.audit.record(&AuditEvent {
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

/// PAN-283: an action that can take control (`Arm`, `Qsy`, `SetSplit`,
/// `TxRequest`, `TakeControl`) needs a currently-valid qsy-or-higher hello
/// capability, checked before any gate or side effect. Returns `None` when the
/// peer holds one. Otherwise logs `verb`, records an audit `TxDenied` with
/// `audit_detail` when given, and returns the refusal: no frame (a peer
/// without a live hello may be sent nothing, PAN-95), no controller change, no
/// owed cleanup.
fn refuse_without_control_capability(
    ctx: &ArmContext,
    peer: &str,
    verb: &str,
    audit_detail: Option<String>,
    now: ArmTime,
) -> Option<DispatchOutcome> {
    if peer_grants(ctx, peer, "qsy", now.unix_ms) {
        return None;
    }
    warn!(target: "agent.control", peer = %peer, verb,
        "refused — no currently-valid qsy-scoped hello capability for this peer");
    if let Some(detail) = audit_detail {
        ctx.audit.record(&AuditEvent {
            ts_unix_ms: now.unix_ms,
            kind: AuditKind::TxDenied,
            operator_callsign: None,
            detail,
        });
    }
    Some(DispatchOutcome::cont())
}

async fn remote_qsy_refusal(ctx: &ArmContext, bus: &MessageBus) -> Option<&'static str> {
    // `ptt_active` is the TX worker's flag. Manual PTT and tune key through
    // `tx::send_ptt_command`, which sets bit 1 of `gain_tx_gate` once a key-down
    // is enqueued; Hamlib clears it on an acknowledged bus PTT-off. Bits 2+
    // count key-downs mid-enqueue. Bit 0 is the receive-gain transition.
    if crate::coordinator::spot_call::rig_is_keyed(&ctx.station_activity.ptt_active, bus) {
        return Some("the rig is transmitting");
    }
    // Drop watch::Ref before awaiting the manager, allowing restart publication.
    let manager = ctx.station_activity.qso_manager.borrow().clone();
    if let Some(manager) = manager {
        if manager
            .get_active_qsos()
            .await
            .iter()
            .any(|(_, qso)| qso.metadata.remote_client_key_id.is_none())
        {
            return Some("the station is running a local or autonomous QSO");
        }
    }
    None
}

fn qsy_refused(peer: &str, command: &str, reason: &str) -> DispatchOutcome {
    warn!(target: "agent.control", peer = %peer, reason, command, "remote frequency change refused");
    DispatchOutcome::cont_with(vec![PeerSend {
        to: SendTarget::One(peer.to_string()),
        frame: pancetta_protocol::ServerFrame::Event {
            event: pancetta_protocol::ServerEvent::Error {
                component: "stationAgent".into(),
                message: format!("{command} refused — {reason}"),
            },
        },
    }])
}

/// Dispatch one decrypted control action against the arm + coordinator bus.
///
/// Pure with respect to timing (takes an [`ArmTime`] sample); side effects are the arm
/// mutation, audit writes, and best-effort bus sends. This is the security spine
/// of the component and is unit-tested directly.
///
/// PAN-283: `Arm`, `Qsy`, `SetSplit`, `TxRequest` and `TakeControl` can take
/// control, so each first requires a currently-valid `qsy`-or-higher hello
/// capability ([`refuse_without_control_capability`]); `StopCq` requires a
/// live hello of any scope. Those checks run before the controller gate and
/// before any other side effect, so a refused action grabs nothing, sends no
/// frame and owes no PAN-137 cleanup (`Arm` and `TxRequest` refusals are
/// audited `TxDenied`).
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
/// - `Qsy` / `SetSplit` → after the capability check, the PAN-252 activity
///   refusal and the controller gate, a coordinator `RigControlMessage`
///   (NON-TX rig control).
/// - `TxRequest(_)` → refused (audited `TxDenied`) while this channel's own
///   consent (`remote_tx_enabled`) is off; otherwise audited `TxRequested` +
///   routed into the REAL QSO engine via a `remote_origin = true`
///   `QsoMessage` (P3.4c). A retry `error` is returned instead while a
///   PAN-137 cleanup is owed or when the QSO inbox does not accept the
///   intent. Creating the QSO is allowed; TRANSMISSION is gated — every
///   TransmitRequest the QSO emits is `TxOrigin::Remote` and dropped by the
///   armed-TX gate unless armed. This
///   arm never keys TX directly (no bypass).
/// - `TakeControl` → refused (logged, no frame, no state change) unless the
///   peer holds a currently-valid `qsy`-or-higher hello capability (PAN-253);
///   otherwise moves control, disarms a displaced controller and owes the
///   PAN-137 QSO cleanup. `ReleaseControl` disarms and releases control.
/// - `StopCq` is refused (logged, no frame, no state change) unless the peer
///   holds a currently-valid hello capability of any scope. It is then
///   controller-gated like `TxRequest` and forwards the
///   station-wide `QsoMessage::StopCq` to the QSO engine (PAN-250). It flushes
///   an owed PAN-137 cleanup first but never waits on it, and returns a retry
///   `error` when the QSO inbox does not accept the stop.
/// - `Unsupported` is a logged no-op.
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
            // PAN-283: arming takes control, so it needs a currently-valid
            // qsy-or-higher hello capability, checked before any side effect.
            if let Some(refused) = refuse_without_control_capability(
                ctx,
                peer,
                "arm",
                Some("arm refused: no currently-valid qsy-scoped hello capability".to_string()),
                now,
            ) {
                return refused;
            }
            // Rule 3/4: arming is a control-mutating action — gate on the
            // controller next (implicit grab if unset; refuse a non-controller).
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
                    ctx.audit.record(&AuditEvent {
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
            // PAN-283: the capability check runs first, ahead of the PAN-252
            // activity refusal, so a peer without the tier grabs nothing and
            // learns nothing about rig or QSO activity.
            if let Some(refused) =
                refuse_without_control_capability(ctx, peer, "setFrequency", None, now)
            {
                return refused;
            }
            if let Some(reason) = remote_qsy_refusal(ctx, bus).await {
                return qsy_refused(peer, "setFrequency", reason);
            }
            let grab = match controller_gate(ctx, peer, now) {
                ControllerGate::Proceed(sends) => sends,
                ControllerGate::Refused(sends) => return DispatchOutcome::cont_with(sends),
            };
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
            // PAN-283: capability first, as for `Qsy`.
            if let Some(refused) =
                refuse_without_control_capability(ctx, peer, "setSplit", None, now)
            {
                return refused;
            }
            if let Some(reason) = remote_qsy_refusal(ctx, bus).await {
                return qsy_refused(peer, "setSplit", reason);
            }
            let grab = match controller_gate(ctx, peer, now) {
                ControllerGate::Proceed(sends) => sends,
                ControllerGate::Refused(sends) => return DispatchOutcome::cont_with(sends),
            };
            let tx_frequency = tx_frequency_hz.max(0.0) as u64;
            info!(target: "rig.split", peer = %peer, "remote SetSplit enabled={enabled} tx={tx_frequency} Hz");
            let store = crate::coordinator::forward_split_change(
                bus,
                ComponentId::StationAgent,
                &ctx.split_tx_frequency_hz,
                enabled,
                tx_frequency,
            )
            .await;
            // Echo the authoritative split state to the local TUI chip.
            let status = ComponentMessage::new(
                ComponentId::StationAgent,
                ComponentId::Tui,
                MessageType::SplitStatus { tx_hz: store },
                std::time::Instant::now(),
            );
            if let Err(e) = bus.send_message(status).await {
                debug!(target: "agent.control", "split status echo failed: {e}");
            }
            DispatchOutcome::cont_with(grab)
        }
        ControlAction::TxRequest(kind) => {
            let detail = match &kind {
                TxKind::CallStation { callsign, .. } => format!("callStation {callsign}"),
                TxKind::AnswerCaller { callsign, step, .. } => {
                    format!("answerCaller {callsign} step={step}")
                }
                TxKind::StartCq { offset_hz } => format!("startCq offset={offset_hz}"),
            };
            // PAN-283: a TX request takes control and opens a remote-origin
            // QSO, so it needs a currently-valid qsy-or-higher hello
            // capability, checked before the gate: a refused request grabs
            // nothing, owes no cleanup and creates no QSO.
            if let Some(refused) = refuse_without_control_capability(
                ctx,
                peer,
                "txRequest",
                Some(format!(
                    "txRequest refused: no currently-valid qsy-scoped hello capability ({detail})"
                )),
                now,
            ) {
                return refused;
            }
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
            let (consented, operator_callsign) = match ctx.arm.lock() {
                Ok(st) => (
                    st.channel_consent(ConsentChannel::StationAgent),
                    st.operator_callsign().map(str::to_string),
                ),
                Err(_) => (false, None),
            };
            if !consented {
                ctx.audit.record(&AuditEvent {
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
            ctx.audit.record(&AuditEvent {
                ts_unix_ms: now.unix_ms,
                kind: AuditKind::TxRequested,
                operator_callsign: ctx
                    .arm
                    .lock()
                    .ok()
                    .and_then(|s| s.operator_callsign().map(str::to_string)),
                detail: detail.clone(),
            });
            flush_qso_cleanup(ctx, bus).await;
            if ctx.qso_cleanup_owed.is_some() {
                // A full inbox can drain between two sends. Do not let this
                // intent overtake a cleanup whose enqueue just failed.
                let mut sends = grab;
                sends.push(PeerSend {
                    to: SendTarget::One(peer.to_string()),
                    frame: pancetta_protocol::ServerFrame::Event {
                        event: pancetta_protocol::ServerEvent::Error {
                            component: "stationAgent".into(),
                            message: "control change pending; retry the QSO request".into(),
                        },
                    },
                });
                return DispatchOutcome::cont_with(sends);
            }
            let qso_msg = tx_kind_to_qso_message(kind, peer);
            if !send_qso(bus, qso_msg).await {
                // The cleanup above may have taken the inbox's last slot.
                warn!(target: "agent.tx", peer = %peer, request = %detail, "txRequest not enqueued on the QSO inbox; asking the client to retry");
                let mut sends = grab;
                sends.push(PeerSend {
                    to: SendTarget::One(peer.to_string()),
                    frame: pancetta_protocol::ServerFrame::Event {
                        event: pancetta_protocol::ServerEvent::Error {
                            component: "stationAgent".into(),
                            message: "QSO request not delivered; retry the QSO request".into(),
                        },
                    },
                });
                return DispatchOutcome::cont_with(sends);
            }
            info!(
                target: "agent.tx",
                request = %detail,
                "routed remote TX-initiation to the QSO engine (remote_origin=true, arm-gated)"
            );
            DispatchOutcome::cont_with(grab)
        }
        ControlAction::StopCq => {
            // PAN-250: a control-mutating action (rules 3/4: implicit grab /
            // refuse a non-controller) routed to the same station-wide
            // `QsoMessage::StopCq` the TUI `s` key and WSJT-X HaltTx send.
            // The stop also cancels a local CQ, so it needs a currently-valid
            // hello capability; any scope will do, stopping needs no tier.
            // Checked before the gate so a refused stop grabs nothing.
            if !peer_grants(ctx, peer, "status", now.unix_ms) {
                warn!(target: "agent.control", peer = %peer, "stopCq refused — no currently-valid hello capability for this peer");
                return DispatchOutcome::cont();
            }
            let grab = match controller_gate(ctx, peer, now) {
                ControllerGate::Proceed(sends) => sends,
                ControllerGate::Refused(sends) => return DispatchOutcome::cont_with(sends),
            };
            // Stopping only reduces TX, so it never waits on an owed PAN-137
            // cleanup; flushing first keeps the cleanup ahead of it when both fit.
            flush_qso_cleanup(ctx, bus).await;
            if !send_qso(bus, crate::message_bus::QsoMessage::StopCq).await {
                warn!(target: "agent.tx", peer = %peer, "stopCq not enqueued on the QSO inbox; asking the client to retry");
                let mut sends = grab;
                sends.push(PeerSend {
                    to: SendTarget::One(peer.to_string()),
                    frame: pancetta_protocol::ServerFrame::Event {
                        event: pancetta_protocol::ServerEvent::Error {
                            component: "stationAgent".into(),
                            message: "stop CQ not delivered; retry stopCq".into(),
                        },
                    },
                });
                return DispatchOutcome::cont_with(sends);
            }
            // The client learns the CQ stopped from the QSO engine's next
            // `activeQsos` snapshot, published after the CQ left TX liveness.
            info!(target: "agent.tx", peer = %peer, "routed remote stopCq to the QSO engine");
            DispatchOutcome::cont_with(grab)
        }
        ControlAction::TakeControl => {
            // Rule 1 (PAN-253): taking control can disarm another operator and
            // end its QSOs, so it needs a currently-valid qsy-or-higher hello
            // capability. Checked before any side effect: a refused takeover
            // leaves the controller, its arm and any owed cleanup untouched.
            if let Some(refused) =
                refuse_without_control_capability(ctx, peer, "takeControl", None, now)
            {
                return refused;
            }
            // An authorized grab always succeeds. If it displaces a DIFFERENT
            // controller, disarm that controller's arm FIRST (arms never
            // transfer — the new controller must arm fresh).
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
            // PAN-137: only a different identity owes QSO cleanup.
            if ctx.controller.as_deref() != Some(peer) {
                owe_qso_cleanup(ctx, peer);
            }
            ctx.controller = Some(peer.to_string());
            info!(target: "agent.control", peer = %peer, "control taken");
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
    // The trust publisher takes arm before writing keys. Release this read
    // guard before waiting for arm to keep that lock order consistent.
    drop(client_keys);

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
            // Trust may have been replaced while this grant was verified.
            // The publisher holds arm while replacing keys, so compare the
            // actual verifying key under arm, not just keyId eligibility.
            if ctx.client_keys.read().unwrap().get(client_key_id) != Some(&client_vk) {
                return Err(format!(
                    "device key for client {client_key_id} changed during arm verification"
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
/// operator's TX-initiation into the real QSO engine, plus the PAN-137
/// controller change and the PAN-250 `StopCq`. TX-initiation messages carry
/// `remote_origin = true`, so the QSO's TransmitRequests are `TxOrigin::Remote`
/// and armed-TX gated downstream (this call itself keys nothing). Returns
/// whether the QSO inbox actually accepted the message.
async fn send_qso(bus: &MessageBus, msg: crate::message_bus::QsoMessage) -> bool {
    let m = ComponentMessage::new(
        ComponentId::StationAgent,
        ComponentId::Qso,
        MessageType::QsoMessage(msg),
        std::time::Instant::now(),
    );
    match bus.send_message_checked(m).await {
        Ok(enqueued) => enqueued,
        Err(e) => {
            debug!(target: "agent.control", "qso-control forward failed: {e}");
            false
        }
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
            dx_offset_unknown: false,
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

/// PAN-103: shared handles onto a RUNNING station-agent session's live
/// authorization state, so a saved config edit can revoke access without a
/// restart. `start_station_agent_component` stashes one on the coordinator
/// only when it spawns the real session (never on the inert/drain or
/// `--replay` paths); teardown and the next start clear it. The arm and audit
/// log are the coordinator's own shared instances and are not repeated here.
#[derive(Clone)]
pub(crate) struct StationAgentLiveHandles {
    pub(crate) tx_allow_list: Arc<std::sync::RwLock<HashSet<String>>>,
    pub(crate) tx_arm_allow_list: Arc<std::sync::RwLock<HashSet<String>>>,
    pub(crate) client_keys: Arc<std::sync::RwLock<std::collections::HashMap<String, VerifyingKey>>>,
    pub(crate) revocation_pending: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) authz_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Set right before the coordinator aborts the session for a config
    /// disable, so the session's disarm guard audits `OperatorDisarm`.
    pub(crate) stop_requested: Arc<std::sync::atomic::AtomicBool>,
}

/// PAN-103: remove `removed` keyIds from a running session's authorization
/// sets, using the same publish protocol as `poll_authorizations_loop`:
/// under the arm lock, disarm (`authorization-revoked`) if the armed client
/// was removed and flag `revocation_pending`, then prune the TX-arm set; then
/// prune the admission set and device keys; then bump `authz_generation`
/// AFTER publishing so the session loop's [`reconcile_authorizations`] evicts
/// the removed peers (Noise transport dropped, hello-token `jti`
/// deny-listed, controller released) on its next tick.
///
/// Poisoned locks are entered rather than skipped: a revocation must never be
/// lost to a poisoned lock. Returns whether anything changed (and so whether
/// the generation was bumped).
pub(crate) fn revoke_static_allow_list_entries(
    live: &StationAgentLiveHandles,
    arm: &Mutex<ArmState>,
    audit: &AuditLog,
    removed: &[String],
    now_unix_ms: i64,
) -> bool {
    if removed.is_empty() {
        return false;
    }
    let mut effects = Vec::new();
    let mut st = arm.lock().unwrap_or_else(|p| p.into_inner());
    let arm_changed = {
        let armed_removed = st
            .armed_client_key_id()
            .is_some_and(|cur| removed.iter().any(|r| r == cur));
        if armed_removed {
            // PAN-104: the audit record is written after the locks are
            // released and the revocation is published, never under them.
            effects = st.disarm_with_reason(DisarmReason::AuthorizationRevoked, now_unix_ms);
            live.revocation_pending.store(true, Ordering::Release);
            warn!(
                target: "agent.tx",
                "remote TX disarmed: the armed client was removed from tx_allow_list"
            );
        }
        // Published while still holding the arm lock (the same atomicity
        // `verify_and_arm`'s re-check relies on for the poller's publish).
        let mut g = match live.tx_arm_allow_list.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = g.len();
        g.retain(|k| !removed.contains(k));
        before != g.len()
    };
    let allow_changed = {
        let mut g = match live.tx_allow_list.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = g.len();
        g.retain(|k| !removed.contains(k));
        before != g.len()
    };
    {
        let mut g = match live.client_keys.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        g.retain(|k, _| !removed.contains(k));
    }
    let changed = arm_changed || allow_changed;
    if changed {
        live.authz_generation.fetch_add(1, Ordering::AcqRel);
    }
    drop(st);
    apply_arm_effects(audit, &effects);
    changed
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
/// Static mode seeds only locally trusted entries from the configured list.
fn seed_tx_arm_allow_list(
    static_tx_allow_list: &HashSet<String>,
    cqdx_will_poll: bool,
    keys: &std::collections::HashMap<String, VerifyingKey>,
) -> HashSet<String> {
    if cqdx_will_poll {
        HashSet::new()
    } else {
        static_tx_allow_list
            .iter()
            .filter(|k| keys.contains_key(*k))
            .cloned()
            .collect()
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
        // PAN-103: set again below only when the real session is spawned.
        self.station_agent_live = None;
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

        // clients/<keyId>.pub is the local trust root, written by pancetta trust-client.
        let tx_allow_list: HashSet<String> = cfg.tx_allow_list.iter().cloned().collect();
        let client_keys = load_client_device_keys(&key_dir, &tx_allow_list);
        // Cloud mode starts fail-closed until the first successful owner-edge poll.
        let cqdx_will_poll =
            cqdx_cfg.enabled && cqdx_cfg.token.as_ref().is_some_and(|t| !t.is_empty());
        let tx_arm_allow_list = Arc::new(std::sync::RwLock::new(seed_tx_arm_allow_list(
            &tx_allow_list,
            cqdx_will_poll,
            &client_keys,
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
        let split_tx_frequency_hz = self.split_tx_frequency_hz();
        let station_activity = StationActivity {
            ptt_active: self.ptt_active.clone(),
            qso_manager: self.qso_manager_watch.subscribe(),
        };
        // Round-1 review fix (PR #261): shared with `poll_authorizations_loop`
        // so a poll-driven disarm can flag the session loop to broadcast the
        // updated control state (see `ArmContext::revocation_pending`).
        let revocation_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // PAN-96: bumped by the poll task when an authorization refresh
        // changes the admission lists; the session loop reconciles on it (see
        // `ArmContext::authz_generation`).
        let authz_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // PAN-103: set by a config disable just before the abort (see
        // `RemoteTxDisarmGuard`).
        let stop_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));

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
        } else {
            let live = StationAgentLiveHandles {
                tx_allow_list: tx_allow_list.clone(),
                tx_arm_allow_list: tx_arm_allow_list.clone(),
                client_keys: client_keys.clone(),
                revocation_pending: revocation_pending.clone(),
                authz_generation: authz_generation.clone(),
                stop_requested: stop_requested.clone(),
            };
            let refresh_arm = arm.clone();
            let refresh_audit = audit.clone();
            let refresh_shutdown = shutdown.clone();
            let refresh_dir = key_dir.clone();
            let interval = Duration::from_secs(cqdx_cfg.authorizations_poll_interval_secs);
            let task = tokio::spawn(async move {
                local_trust_refresh_loop(
                    live,
                    refresh_arm,
                    refresh_audit,
                    refresh_dir,
                    refresh_shutdown,
                    interval,
                )
                .await;
            });
            self.station_agent_poll = Some(task.abort_handle());
        }

        // PAN-103: live handles for config-edit revocation (see
        // `coordinator::config_reload`).
        self.station_agent_live = Some(StationAgentLiveHandles {
            tx_allow_list: tx_allow_list.clone(),
            tx_arm_allow_list: tx_arm_allow_list.clone(),
            client_keys: client_keys.clone(),
            revocation_pending: revocation_pending.clone(),
            authz_generation: authz_generation.clone(),
            stop_requested: stop_requested.clone(),
        });

        let handle = tokio::spawn(async move {
            run_session_loop(RunConfig {
                station_activity,
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
                split_tx_frequency_hz,
                revocation_pending,
                authz_generation,
                shutdown,
                stop_requested,
                timeouts: net::RelayTimeouts::default(),
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

/// The default agent key directory: `<data-dir>/agent` (PAN-376:
/// `--data-dir`, else `~/.pancetta`). Also used by the `pancetta pair` CLI
/// subcommand so both paths resolve the same directory.
pub fn default_key_dir() -> std::path::PathBuf {
    pancetta_core::data_dir::current().join("agent")
}

/// Load client device verifying keys from `key_dir/clients/<keyId>.pub` (raw
/// 32-byte Ed25519), for each allow-listed keyId. Missing/invalid files are
/// skipped (that client then fails closed at verify time). Written by
/// `pancetta trust-client`; absent local trust, no client can arm.
fn load_client_device_keys(
    key_dir: &std::path::Path,
    allow: &HashSet<String>,
) -> std::collections::HashMap<String, VerifyingKey> {
    let mut out = std::collections::HashMap::new();
    let dir = key_dir.join("clients");
    for kid in allow {
        if pancetta_agent::trust::validate_key_id(kid).is_err() {
            continue;
        }
        let path = dir.join(format!("{kid}.pub"));
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

type ClientKeys = std::collections::HashMap<String, VerifyingKey>;
type AuthorizationSnapshot = (HashSet<String>, HashSet<String>, ClientKeys);

/// Publish eligibility and disarm under the same lock used at arm time.
/// Without a fresh cloud snapshot, read current admission under that lock so
/// a concurrent config removal cannot be restored from a stale snapshot.
/// Cached owner edges distinguish cloud refresh from static mode.
fn publish_trust(
    live: &StationAgentLiveHandles,
    arm: &Mutex<ArmState>,
    audit: &AuditLog,
    cloud: Option<AuthorizationSnapshot>,
    cached_owners: Option<&HashSet<String>>,
    key_dir: &std::path::Path,
) {
    let mut st = arm.lock().unwrap_or_else(|p| p.into_inner());
    let is_cloud = cloud.is_some() || cached_owners.is_some();
    let (admission, candidates, keys) = cloud.unwrap_or_else(|| {
        let admission = live
            .tx_allow_list
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let keys = load_client_device_keys(key_dir, &admission);
        let candidates = cached_owners.cloned().unwrap_or_else(|| admission.clone());
        (admission, candidates, keys)
    });
    let eligible: HashSet<String> = candidates
        .iter()
        .filter(|k| keys.contains_key(*k))
        .cloned()
        .collect();
    let mut effects = Vec::new();
    let current_keys = live.client_keys.read().unwrap_or_else(|p| p.into_inner());
    if let Some(kid) = st
        .armed_client_key_id()
        .filter(|kid| !eligible.contains(*kid) || current_keys.get(*kid) != keys.get(*kid))
    {
        let reason = if candidates.contains(kid) {
            DisarmReason::LocalTrustRevoked
        } else {
            DisarmReason::AuthorizationRevoked
        };
        effects = st.disarm_with_reason(reason, now_ms());
        live.revocation_pending.store(true, Ordering::Release);
    }
    drop(current_keys);
    let mut current = live
        .tx_arm_allow_list
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let mut changed = *current != eligible;
    *current = eligible;
    drop(current);
    if is_cloud {
        let mut current = live
            .tx_allow_list
            .write()
            .unwrap_or_else(|p| p.into_inner());
        changed |= *current != admission;
        *current = admission;
    }
    let mut current_keys = live.client_keys.write().unwrap_or_else(|p| p.into_inner());
    changed |= *current_keys != keys;
    *current_keys = keys;
    drop(current_keys);
    if changed {
        live.authz_generation.fetch_add(1, Ordering::AcqRel);
    }
    drop(st);
    apply_arm_effects(audit, &effects);
    if is_cloud {
        info!(target: "agent", "txArmSet = localTrust ∩ owner edges");
    } else {
        info!(target: "agent", "txArmSet = localTrust ∩ static tx_allow_list");
    }
}

async fn local_trust_refresh_loop(
    live: StationAgentLiveHandles,
    arm: Arc<Mutex<ArmState>>,
    audit: AuditLog,
    key_dir: std::path::PathBuf,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    interval: Duration,
) {
    while !shutdown.load(Ordering::Acquire) {
        publish_trust(&live, &arm, &audit, None, None, &key_dir);
        tokio::time::sleep(interval).await;
    }
}

/// Periodically refresh the shared `tx_allow_list`/`tx_arm_allow_list`/
/// `client_keys` from cqdx's live authorization data (dispensa Q-0043
/// auto-populate; the `tx_arm_allow_list` split is CQD-11 / dispensa Q-0052).
/// Runs for the lifetime of the station-agent component; only spawned when
/// cqdx integration is enabled with a token configured (see
/// `start_station_agent_component`).
///
/// Fail-safe: any poll failure (network error, non-2xx status including a
/// 404, malformed JSON) is logged at WARN and retains cloud admission and
/// owner edges. Local trust is refreshed independently every interval, even
/// while a request is pending, so an outage cannot postpone local untrust.
/// Only a successful poll replaces cloud authority; an empty response revokes it.
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
/// Local trust changes also bump the generation. A failed poll alone does not.
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

    let live = StationAgentLiveHandles {
        tx_allow_list,
        tx_arm_allow_list,
        client_keys,
        revocation_pending,
        authz_generation,
        stop_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    // Keep full owner edges, not the locally intersected eligibility set.
    // Before the first successful response the cloud seed remains fail-closed.
    let mut owner_edges: Option<HashSet<String>> = None;
    let mut refresh = tokio::time::interval(interval);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let client = &client;
    let fetch = |delay| async move {
        tokio::time::sleep(delay).await;
        client.fetch_authorizations().await
    };
    let mut poll = Box::pin(fetch(Duration::ZERO));
    while !shutdown.load(Ordering::Acquire) {
        tokio::select! {
        _ = refresh.tick() => {
            if let Some(owners) = &owner_edges {
                publish_trust(&live, &arm, &audit, None, Some(owners), &key_dir);
            }
        }
        result = &mut poll => {
        match result {
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
                owner_edges = Some(new_tx_arm_allow.clone());
                publish_trust(
                    &live,
                    &arm,
                    &audit,
                    Some((new_allow, new_tx_arm_allow, new_keys)),
                    None,
                    &key_dir,
                );
            }
            Err(e) => {
                warn!(
                    target: "agent",
                    "authorizations poll failed: {e}; retaining cloud authority and refreshing local trust, retrying in {:?}",
                    interval
                );
            }
        }
        poll = Box::pin(fetch(interval));
        }
        }
    }
}

/// Everything the session loop owns.
struct RunConfig {
    station_activity: StationActivity,
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
    /// Threaded into `ArmContext::split_tx_frequency_hz` — see there.
    split_tx_frequency_hz: Arc<std::sync::atomic::AtomicU64>,
    /// Threaded into `ArmContext::revocation_pending` — see there.
    revocation_pending: Arc<std::sync::atomic::AtomicBool>,
    /// Threaded into `ArmContext::authz_generation` — see there.
    authz_generation: Arc<std::sync::atomic::AtomicU64>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// See [`StationAgentLiveHandles::stop_requested`].
    stop_requested: Arc<std::sync::atomic::AtomicBool>,
    /// Deadlines for every relay IO wait (PAN-102). `Default` in production.
    timeouts: net::RelayTimeouts,
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
        stop_requested: cfg.stop_requested.clone(),
        normal_exit: false,
    };
    let mut backoff = RECONNECT_BACKOFF_MIN;
    let mut ctx = ArmContext {
        station_activity: cfg.station_activity,
        arm: cfg.arm.clone(),
        split_tx_frequency_hz: cfg.split_tx_frequency_hz.clone(),
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
        #[cfg(test)]
        _audit_tmp: None,
        // Not pinned to any fixed peer — established peers are learned and
        // vetted fresh each session by `MultiPeerSession` itself (see
        // `run_one_session`), so a different set of allow-listed clients can
        // connect on the next reconnect.
        peers: std::collections::HashMap::new(),
        // No controller until a peer grabs it (explicitly or implicitly).
        controller: None,
        qso_cleanup_owed: None,
        revocation_pending: cfg.revocation_pending,
        authz_generation: cfg.authz_generation,
        // Set from the live generation at the start of every session.
        authz_generation_seen: 0,
        clock: Arc::new(ArmTime::now),
        shutdown: cfg.shutdown.clone(),
    };

    while !cfg.shutdown.load(Ordering::Acquire) {
        match net::RealWsConn::connect(&cfg.relay_url, cfg.timeouts).await {
            Ok(ws) => {
                backoff = RECONNECT_BACKOFF_MIN;
                run_one_session(
                    ws,
                    &cfg.identity,
                    &mut ctx,
                    &cfg.bus,
                    &mut cfg.events,
                    cfg.timeouts.hello,
                )
                .await;
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
/// (PAN-95). A failed serialize or per-peer send is `debug!`-logged and
/// skipped (best-effort — a control frame that can't reach one peer never
/// tears down the session). A relay transport failure is different: the
/// socket is shared, so [`run_one_session`] ends the session at its next
/// check (PAN-102).
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

/// The display feed queues frames only while the relay's outbound queue has
/// this many free slots. One fan-out needs at most `MAX_PEERS`, so a display
/// fan-out never waits and leaves at least `MAX_PEERS` free slots, enough for
/// one control fan-out (PAN-102). Control frames are not prioritised: the
/// queue is one FIFO, so a control frame can sit behind up to
/// `RELAY_QUEUE_DEPTH - DISPLAY_FEED_MIN_SEND_CAPACITY + MAX_PEERS` (56) queued
/// display frames. In a control batch larger than `MAX_PEERS`, each send after
/// the first `MAX_PEERS` can wait up to the relay send deadline.
const DISPLAY_FEED_MIN_SEND_CAPACITY: usize = 2 * pancetta_agent::multi_session::MAX_PEERS;
const _: () = assert!(DISPLAY_FEED_MIN_SEND_CAPACITY < net::RELAY_QUEUE_DEPTH);

/// Fan pending display events out to every established peer that
/// [`peer_may_receive`] admits, reading the clock for each recipient check
/// (PAN-95: a peer without an unexpired `status`+ hello capability gets
/// nothing). Each call consumes at most [`READ_STREAM_DRAIN_MAX`] events or
/// lag notifications before returning to session housekeeping (PAN-97: so a
/// busy read stream cannot hold the arm-expiry sweep off). Lossy by design:
/// Lagged means the bounded broadcast ring dropped old events; log and
/// continue within the same batch limit. The drain also stops while the
/// relay's outbound queue has fewer than [`DISPLAY_FEED_MIN_SEND_CAPACITY`]
/// free slots (PAN-102): pending events stay in the broadcast ring, which
/// drops the oldest if the congestion persists, and the drain never waits to
/// send.
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
        // Checked before taking an event, so no event is taken and then
        // dropped, and no frame is encrypted and then left unsent.
        if sess
            .send_capacity()
            .is_some_and(|free| free < DISPLAY_FEED_MIN_SEND_CAPACITY)
        {
            // Leave the rest in the broadcast ring; it drops the oldest if this persists.
            break;
        }
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
/// seam is synchronous (`RealWsConn` bridges via `block_on`). Every relay wait
/// is deadline-bounded (PAN-102): the opening `hello` must arrive within
/// `hello_timeout`, and `RealWsConn` bounds each send and receive with its
/// [`net::RelayTimeouts`], so a relay that stops responding ends the session
/// instead of holding it.
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
    hello_timeout: Duration,
) {
    ctx.peers.clear();
    // D8 (PAN-96): record the generation BEFORE snapshotting the list. A poll
    // that publishes after the snapshot bumps afterwards and triggers a
    // reconcile; one landing between the two costs one harmless extra one.
    ctx.authz_generation_seen = ctx.authz_generation.load(Ordering::Acquire);
    let allow_snapshot = ctx.tx_allow_list.read().unwrap().clone();
    let mut sess = MultiPeerSession::new(ws, identity, allow_snapshot);
    if let Err(e) = sess.authenticate(hello_timeout) {
        debug!(target: "agent", "relay authenticate failed: {e}");
        return;
    }
    loop {
        // PAN-102: `process_next` blocks synchronously (`block_in_place`) for
        // up to `RECV_TICK`, and a quiet relay makes every branch below
        // synchronous too, so without this the loop would never reach an
        // `.await`: an `abort()` (shutdown.rs's force-stop, supervisor
        // teardown) is only observed at a yield point, and would wait for
        // the next socket event while a remote-TX arm stays live. Yield once
        // per tick, and end the session at once when shutdown is requested;
        // the caller disarms.
        tokio::task::yield_now().await;
        if ctx.shutdown.load(Ordering::Acquire) {
            debug!(target: "agent", "shutdown requested; ending relay session");
            return;
        }
        flush_qso_cleanup(ctx, bus).await;
        let polled = sess.process_next(RECV_TICK);
        // PAN-97: expire a lapsed arm on EVERY iteration, before acting on
        // what was received — time spent receiving counts, and no branch below
        // (valid traffic, malformed frames, idle ticks) can skip it. The
        // TX worker's key-time gate still checks the clock independently.
        let now = (ctx.clock)();
        let expiry_sends = sweep_arm_expiry(ctx, now);
        deliver_sends(&mut sess, ctx, &expiry_sends, || (ctx.clock)().unix_ms);
        // PAN-102: the relay socket is shared by every peer. Once a send on it has
        // failed, end the session before acting on anything received; the caller
        // disarms and reconnects.
        if sess.transport_failed() {
            debug!(target: "agent", "relay transport failed; ending session");
            return;
        }
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
#[path = "tests/tests.rs"]
mod tests;
