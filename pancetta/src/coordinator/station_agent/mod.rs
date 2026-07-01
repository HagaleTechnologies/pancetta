//! Station-agent component — the authenticated remote-control transport.
//!
//! This component owns the **paired, Noise-encrypted control channel** to a
//! remote client (via the cqdx relay Durable Object) and is the ONLY place a
//! decrypted client control frame drives the coordinator. It is **default-OFF**
//! and **inert** unless the operator has both enabled it in config AND completed
//! pairing — a stock station is byte-identical to one built before this
//! component existed.
//!
//! ## What this component does (P3.4b)
//!
//! - **Connect + authenticate + Noise handshake** to the relay
//!   ([`net::RealWsConn`] → [`AgentSession`]).
//! - **Verify + arm** on a `txArmGrant`: the two-stage crown-jewel verification
//!   ([`CapabilityVerifier`]) mints a [`VerifiedArmGrant`] which is fed into the
//!   coordinator's shared `remote_tx_arm` [`ArmState`]. Once armed (AND the
//!   operator's local `remote_tx_enabled` consent is ON), a `TxOrigin::Remote`
//!   transmit request will key PTT at the TX worker's arm gate.
//! - **Heartbeat / disarm / control-loss** all drive the same `ArmState`:
//!   a heartbeat slides the dead-man window; an explicit `Disarm`, a peer
//!   `down` presence, a session teardown, or any terminal error **disarms**
//!   (fail TX-off on control-channel loss, Part-97).
//! - **Non-TX rig control** (`Qsy`, `SetSplit`) is forwarded onto the existing
//!   coordinator bus (`RigControlMessage`) — read/QSY/split only.
//! - **TX-initiation** (`callStation` / `answerCaller` / `startCq`) is
//!   **audited but deferred to P3.4c** — v1 does NOT route these through the QSO
//!   engine; each is logged "not-yet-supported in v1" and does NOT key TX.
//! - **Read stream (minimal v1)**: decoded frames + scalar status are
//!   translated (`remote_gateway::translate`) and sent back as encrypted `env`
//!   frames, drained opportunistically between control-frame reads.
//!
//! ## Inert-when-off invariant
//!
//! Disabled OR unpaired OR missing relay/pairing URL → the component spawns a
//! no-op drain task (so additive bus sends never flood) and does nothing else.
//! Local consent is still seeded into the arm from config at startup (so the
//! gate reflects `remote_tx_enabled` even when the transport is off), matching
//! the coordinator's constructor seeding — this is idempotent.

pub(crate) mod net;

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ed25519_dalek::VerifyingKey;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use pancetta_agent::arm::{ArmEffect, ArmState};
use pancetta_agent::audit::{AuditEvent, AuditKind, AuditLog};
use pancetta_agent::capability::CapabilityVerifier;
use pancetta_agent::control::{map_client_frame, ControlAction, TxKind};
use pancetta_agent::keys::AgentIdentity;
use pancetta_agent::pairing::{IdpKey, PairedState};
use pancetta_agent::session::AgentSession;

use crate::message_bus::{
    ComponentId, ComponentMessage, MessageBus, MessageType, RigControlMessage,
};

/// Reconnect backoff (capped) after a transient session teardown.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_secs(2);
/// Cap on the reconnect backoff.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Everything the dispatch loop needs to act on a decrypted control action:
/// the shared arm, the capability verifier, the client's device verifying key,
/// the station-local TX-allow-list, the session replay set, and the audit log.
///
/// Held on the blocking session task; the arm is the coordinator's shared
/// `remote_tx_arm`, so arming here is visible to the TX worker immediately.
struct ArmContext {
    arm: Arc<Mutex<ArmState>>,
    verifier: CapabilityVerifier,
    /// Verifying keys for allow-listed clients, keyed by client keyId. The
    /// grant's `clientSig` is checked against the key matching its `clientKeyId`.
    client_keys: std::collections::HashMap<String, VerifyingKey>,
    tx_allow_list: HashSet<String>,
    seen_jtis: HashSet<String>,
    audit: AuditLog,
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
        }
    }
}

/// Unix milliseconds now (the one clock read for arm timing; `ArmState` is pure).
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
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

/// Dispatch one decrypted control action against the arm + coordinator bus.
///
/// Pure with respect to timing (takes `now_ms`); side effects are the arm
/// mutation, audit writes, and best-effort bus sends. This is the security spine
/// of the component and is unit-tested directly.
///
/// - `Arm` → verify capability + grant (fail-closed) → `arm.arm()` on success,
///   audited `TxDenied` on any verification error (NEVER arms on failure).
/// - `Heartbeat` → `arm.heartbeat()` (slides the dead-man window).
///   TODO(P3.4c): enforce `seq` monotonicity + `arm_jti` match (contract
///   `$defs.txHeartbeat`).
/// - `Disarm` → `arm.disarm()` + audit.
/// - `Qsy` / `SetSplit` → coordinator `RigControlMessage` (NON-TX rig control).
/// - `TxRequest(_)` → audited `TxRequested` + logged "not supported in v1";
///   does NOT key TX (deferred to P3.4c).
/// - `StopCq` / `TakeControl` / `ReleaseControl` / `Unsupported` → logged no-op
///   in v1.
async fn dispatch_action(
    action: ControlAction,
    ctx: &mut ArmContext,
    bus: &MessageBus,
    now: i64,
) -> Dispatch {
    match action {
        ControlAction::Arm { grant } => {
            match verify_and_arm(&grant, ctx, now) {
                Ok(()) => {}
                Err(reason) => {
                    // Fail-closed: audit the denial, do NOT arm.
                    ctx.audit.append(&AuditEvent {
                        ts_unix_ms: now,
                        kind: AuditKind::TxDenied,
                        operator_callsign: None,
                        detail: format!("arm rejected: {reason}"),
                    });
                    warn!(target: "agent.tx", reason = %reason, "remote arm grant rejected");
                }
            }
            Dispatch::Continue
        }
        ControlAction::Heartbeat { arm_jti, seq } => {
            // TODO(P3.4c): reject seq <= last-accepted for this arm_jti + require
            // arm_jti == the current armed grant's jti (contract $defs.txHeartbeat).
            let _ = (arm_jti, seq);
            if let Ok(mut st) = ctx.arm.lock() {
                st.heartbeat(now);
            }
            Dispatch::Continue
        }
        ControlAction::Disarm => {
            let effects = match ctx.arm.lock() {
                Ok(mut st) => st.disarm(now),
                Err(_) => Vec::new(),
            };
            apply_arm_effects(&ctx.audit, &effects);
            Dispatch::Continue
        }
        ControlAction::Qsy { vfo, frequency_hz } => {
            let msg = RigControlMessage::SetFrequency {
                vfo: vfo.clamp(0, u8::MAX as i64) as u8,
                frequency: frequency_hz.max(0.0) as u64,
            };
            send_rig(bus, msg).await;
            Dispatch::Continue
        }
        ControlAction::SetSplit {
            enabled,
            tx_frequency_hz,
        } => {
            let msg = RigControlMessage::SetSplit {
                enabled,
                tx_frequency: tx_frequency_hz.max(0.0) as u64,
            };
            send_rig(bus, msg).await;
            Dispatch::Continue
        }
        ControlAction::TxRequest(kind) => {
            // DEFERRED to P3.4c: v1 does not route remote TX-initiation through
            // the QSO engine. Audit the intent + log; never key TX.
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
                detail: format!("remote TX-initiation not supported in v1 (P3.4c): {detail}"),
            });
            warn!(
                target: "agent.tx",
                request = %detail,
                "remote TX-initiation not supported in v1 (deferred to P3.4c); ignoring"
            );
            Dispatch::Continue
        }
        ControlAction::StopCq | ControlAction::TakeControl | ControlAction::ReleaseControl => {
            debug!(target: "agent.control", action = action_name(&action), "control action not wired in v1");
            Dispatch::Continue
        }
        ControlAction::Unsupported => {
            debug!(target: "agent.control", "ignoring unsupported control frame");
            Dispatch::Continue
        }
    }
}

/// A short static name for the no-op control actions (diagnostics only).
fn action_name(a: &ControlAction) -> &'static str {
    match a {
        ControlAction::StopCq => "stopCq",
        ControlAction::TakeControl => "takeControl",
        ControlAction::ReleaseControl => "releaseControl",
        _ => "other",
    }
}

/// Verify a raw `txArmGrant` (client-signed) and arm the shared `ArmState`.
///
/// Fail-closed: any verification error returns `Err(reason)` and the caller
/// audits a `TxDenied` without arming. On success, `ArmState::arm` is called
/// (which itself audits `Armed`, or refuses + audits a no-tx-scope grant).
///
/// The grant must carry a `clientKeyId` present in the station-local
/// TX-allow-list AND for which we hold a device verifying key; its `clientSig`
/// is verified against that key. The capability half is verified from the grant
/// via the pinned IdP keys — v1 expects the client to send the capabilityToken
/// **inside** the grant object under `capabilityToken` (e2e-auth.v1 §4). If it
/// is absent we fail closed.
fn verify_and_arm(grant: &serde_json::Value, ctx: &mut ArmContext, now: i64) -> Result<(), String> {
    let obj = grant.as_object().ok_or("grant is not a JSON object")?;

    // The client keyId this grant claims — used to pick the device key AND
    // gate on the station-local allow-list before any crypto.
    let client_key_id = obj
        .get("clientKeyId")
        .and_then(|v| v.as_str())
        .ok_or("grant missing clientKeyId")?;
    if !ctx.tx_allow_list.contains(client_key_id) {
        return Err(format!(
            "client {client_key_id} not in station-local TX-allow-list"
        ));
    }
    let client_vk = *ctx
        .client_keys
        .get(client_key_id)
        .ok_or_else(|| format!("no device key for client {client_key_id}"))?;

    // The capabilityToken (compact JWS) rides inside the grant in v1.
    let token = obj
        .get("capabilityToken")
        .and_then(|v| v.as_str())
        .ok_or("grant missing capabilityToken")?;
    let cap = ctx
        .verifier
        .verify_capability_token(token, now)
        .map_err(|e| format!("capability: {e}"))?;

    let verified = ctx
        .verifier
        .verify_arm_grant(
            grant,
            &cap,
            &client_vk,
            &ctx.tx_allow_list,
            now,
            &mut ctx.seen_jtis,
        )
        .map_err(|e| format!("arm grant: {e}"))?;

    // Arm the shared state (audits Armed, or refuses a no-scope grant).
    let effects = match ctx.arm.lock() {
        Ok(mut st) => st.arm(verified, now),
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

impl super::ApplicationCoordinator {
    /// Start the station-agent component (default-OFF, inert unless enabled +
    /// paired). Mirrors [`start_remote_gateway_component`](super::ApplicationCoordinator::start_remote_gateway_component):
    /// disabled/unpaired → drain-only; enabled + paired → connect + serve.
    pub(crate) async fn start_station_agent_component(&mut self) -> Result<()> {
        let config = self.config.read().await;
        let cfg = config.network.station_agent.clone();
        drop(config);

        // Seed local consent from config regardless of enabled/paired, so the
        // arm reflects `remote_tx_enabled` even when the transport is off. This
        // mirrors (idempotently) the coordinator-constructor seeding.
        {
            let now = now_ms();
            if let Ok(mut st) = self.remote_tx_arm.lock() {
                let _ = st.set_local_consent(cfg.remote_tx_enabled, now);
            }
        }

        // --- Inert paths: disabled or missing required config ---------------
        let (relay_url, pairing_api_url) = match (&cfg.relay_url, &cfg.pairing_api_url) {
            (Some(r), Some(p)) if cfg.enabled && !r.is_empty() && !p.is_empty() => {
                (r.clone(), p.clone())
            }
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
                info!(
                    target: "agent",
                    "station agent enabled but not paired — run pairing (operator action); staying idle"
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

        let audit = AuditLog::new(
            cfg.audit_log_path
                .clone()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(pancetta_agent::audit::default_audit_path),
        );

        // Learn the client peer keyId for env addressing. v1 uses the FIRST
        // allow-listed client keyId as the expected peer (single-client v1).
        let client_key_id = match tx_allow_list.iter().next().cloned() {
            Some(c) => c,
            None => {
                info!(
                    target: "agent",
                    "station agent paired but tx_allow_list is empty — no client to admit; idle"
                );
                return self.spawn_station_agent_drain().await;
            }
        };

        let bus = self.message_bus.clone();
        let shutdown = self.shutdown_signal.clone();
        let arm = self.remote_tx_arm.clone();

        // Drain channel so additive bus sends addressed to StationAgent never
        // flood (parity with the gateway).
        let (_sa_tx, _sa_rx) = self
            .message_bus
            .create_channel(ComponentId::StationAgent)
            .await?;

        let handle = tokio::spawn(async move {
            run_session_loop(RunConfig {
                relay_url,
                identity,
                client_key_id,
                verifier,
                client_keys,
                tx_allow_list,
                audit,
                bus,
                arm,
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
            .create_channel(ComponentId::StationAgent)
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

/// The default agent key directory: `~/.pancetta/agent`.
fn default_key_dir() -> std::path::PathBuf {
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

/// Everything the session loop owns.
struct RunConfig {
    relay_url: String,
    identity: AgentIdentity,
    client_key_id: String,
    verifier: CapabilityVerifier,
    client_keys: std::collections::HashMap<String, VerifyingKey>,
    tx_allow_list: HashSet<String>,
    audit: AuditLog,
    bus: MessageBus,
    arm: Arc<Mutex<ArmState>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

/// The connect → authenticate → process → (disarm on loss) → reconnect loop.
///
/// On any session teardown the arm is disarmed (fail TX-off on control-channel
/// loss, Part-97), then the loop reconnects with capped backoff until shutdown.
async fn run_session_loop(cfg: RunConfig) {
    let mut backoff = RECONNECT_BACKOFF_MIN;
    let mut ctx = ArmContext {
        arm: cfg.arm.clone(),
        verifier: cfg.verifier,
        client_keys: cfg.client_keys,
        tx_allow_list: cfg.tx_allow_list,
        seen_jtis: HashSet::new(),
        audit: cfg.audit,
    };

    while !cfg.shutdown.load(Ordering::Acquire) {
        match net::RealWsConn::connect(&cfg.relay_url).await {
            Ok(ws) => {
                backoff = RECONNECT_BACKOFF_MIN;
                run_one_session(ws, &cfg.identity, &cfg.client_key_id, &mut ctx, &cfg.bus).await;
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

/// A monotonic ordinal for the session's handshake progress, used to tell a
/// benign frame (phase advanced) from a drained socket (phase unchanged) when
/// `process_next` returns `Ok(None)`. Only ever increases as the leg advances:
/// pre-admit (0) → admitted (1) → transport (2).
fn session_phase<W: pancetta_agent::relay::WsConn>(sess: &AgentSession<'_, W>) -> u8 {
    if sess.is_transport_established() {
        2
    } else if sess.is_admitted() {
        1
    } else {
        0
    }
}

/// Disarm the shared arm on any control-channel loss and audit it.
fn disarm_on_loss(ctx: &mut ArmContext) {
    let effects = match ctx.arm.lock() {
        Ok(mut st) => st.disarm(now_ms()),
        Err(_) => Vec::new(),
    };
    if !effects.is_empty() {
        debug!(target: "agent.tx", "control channel lost — disarming remote TX");
    }
    apply_arm_effects(&ctx.audit, &effects);
}

/// Drive one session to completion: auth → handshake → dispatch control frames.
/// Returns when the session tears down (drain, teardown action, or error).
///
/// This runs on a blocking-capable task because the [`WsConn`] seam is
/// synchronous (`RealWsConn` bridges via `block_on`).
async fn run_one_session<W: pancetta_agent::relay::WsConn>(
    ws: W,
    identity: &AgentIdentity,
    client_key_id: &str,
    ctx: &mut ArmContext,
    bus: &MessageBus,
) {
    let mut sess = AgentSession::new(ws, identity, client_key_id.to_string());
    if let Err(e) = sess.authenticate() {
        debug!(target: "agent", "relay authenticate failed: {e}");
        return;
    }
    // Distinguish a benign frame (ready/presence/msg1 → `Ok(None)`, session phase
    // advances) from a drained/closed socket (`recv_text` → `None` → `Ok(None)`,
    // no phase change). A `None` with no forward progress is a close: tear down.
    let mut last_phase = session_phase(&sess);
    loop {
        match sess.process_next() {
            Ok(Some(plaintext)) => {
                let action = match map_client_frame(&plaintext) {
                    Ok(a) => a,
                    Err(e) => {
                        debug!(target: "agent.control", "malformed control frame: {e}");
                        continue;
                    }
                };
                if dispatch_action(action, ctx, bus, now_ms()).await == Dispatch::Teardown {
                    return;
                }
                last_phase = session_phase(&sess);
            }
            Ok(None) => {
                let phase = session_phase(&sess);
                if phase == last_phase {
                    // No forward progress → `recv_text` drained/closed the socket.
                    return;
                }
                last_phase = phase;
            }
            Err(e) => {
                debug!(target: "agent", "session error: {e}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};
    use pancetta_agent::arm::HEARTBEAT_TIMEOUT_MS;
    use serde_json::{json, Value};
    use std::collections::BTreeMap;

    const AGENT_KEY_ID: &str = "agentKeyId000000";
    const CLIENT_KEY_ID: &str = "clientKeyId00000";
    const IDP_KID: &str = "idp-kid-1";
    const OPERATOR: &str = "K5ARH";
    const NOW: i64 = 1_700_000_000_000;

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

    /// Build a valid, client-signed grant carrying the capabilityToken inside it
    /// (v1 convention), with a unique jti so replay tests can vary it.
    fn signed_grant(jti: &str) -> Value {
        let mut grant = json!({
            "type": "txArmGrant",
            "aud": AGENT_KEY_ID,
            "clientKeyId": CLIENT_KEY_ID,
            "sessionId": "sess-1",
            "capabilityJti": "cap-jti-1",
            "capabilityToken": valid_token(),
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
        let sig = client_key().sign(&canon);
        grant.insert("clientSig".to_string(), json!(b64url(&sig.to_bytes())));
        Value::Object(grant)
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
        ArmContext {
            arm: Arc::new(Mutex::new(ArmState::new())),
            verifier: CapabilityVerifier {
                agent_key_id: AGENT_KEY_ID.to_string(),
                pinned_idp_keys: vec![IdpKey {
                    kid: IDP_KID.to_string(),
                    public_key: idp_key().verifying_key().to_bytes(),
                }],
            },
            client_keys,
            tx_allow_list: allow,
            seen_jtis: HashSet::new(),
            audit: AuditLog::new(audit_tmp()),
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
            ControlAction::Arm {
                grant: signed_grant("arm-jti-1"),
            },
            &mut ctx,
            &bus,
            NOW,
        )
        .await;
        assert_eq!(d, Dispatch::Continue);
        assert!(
            ctx.arm.lock().unwrap().tx_permitted(NOW),
            "a valid arm + consent must permit remote TX"
        );
    }

    // ── Case 4: heartbeat loss auto-disarms (dead-man) ──────────────────────
    #[tokio::test]
    async fn heartbeat_loss_disarms_after_timeout() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            ControlAction::Arm {
                grant: signed_grant("arm-jti-1"),
            },
            &mut ctx,
            &bus,
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
            ControlAction::Arm {
                grant: signed_grant("arm-jti-1"),
            },
            &mut ctx2,
            &bus,
            NOW,
        )
        .await;
        dispatch_action(
            ControlAction::Heartbeat {
                arm_jti: "arm-jti-1".into(),
                seq: 1,
            },
            &mut ctx2,
            &bus,
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

    // ── Case 5: consent OFF → even a valid Arm never permits TX ─────────────
    #[tokio::test]
    async fn consent_off_never_permits_even_with_valid_arm() {
        let mut ctx = ctx_with(true, true);
        // remote_tx_enabled is OFF (default): do NOT set consent on.
        let bus = MessageBus::new(64).unwrap();
        dispatch_action(
            ControlAction::Arm {
                grant: signed_grant("arm-jti-1"),
            },
            &mut ctx,
            &bus,
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
            ControlAction::Arm {
                grant: signed_grant("arm-jti-1"),
            },
            &mut ctx,
            &bus,
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
            ControlAction::Arm {
                grant: signed_grant("arm-jti-1"),
            },
            &mut ctx,
            &bus,
            NOW,
        )
        .await;
        assert!(ctx.arm.lock().unwrap().tx_permitted(NOW));
        dispatch_action(ControlAction::Disarm, &mut ctx, &bus, NOW).await;
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
                grant: grant.clone(),
            },
            &mut ctx,
            &bus,
            NOW,
        )
        .await;
        // Disarm, then replay the SAME grant jti — must not re-arm.
        dispatch_action(ControlAction::Disarm, &mut ctx, &bus, NOW).await;
        dispatch_action(ControlAction::Arm { grant }, &mut ctx, &bus, NOW).await;
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "a replayed grant jti must be rejected (single-use)"
        );
    }

    // ── TX-initiation is audited but never keys TX in v1 ────────────────────
    #[tokio::test]
    async fn tx_request_is_deferred_and_never_arms() {
        let mut ctx = ctx_with(true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        let d = dispatch_action(
            ControlAction::TxRequest(TxKind::CallStation {
                callsign: "W1XYZ".into(),
                frequency_hz: 1500.0,
                dx_parity: None,
            }),
            &mut ctx,
            &bus,
            NOW,
        )
        .await;
        assert_eq!(d, Dispatch::Continue);
        assert!(
            !ctx.arm.lock().unwrap().is_armed(),
            "a TX-initiation must not arm or key TX in v1"
        );
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
                .remote_public_key(remote_pub)
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
    }

    /// Mint a valid capabilityToken whose `aud` is `agent_key_id`.
    fn token_for_agent(agent_key_id: &str) -> String {
        let header = json!({ "alg": "EdDSA", "kid": IDP_KID, "typ": "JWT" });
        let payload = json!({
            "iss": "cqdx", "operatorCallsign": OPERATOR,
            "aud": agent_key_id, "clientKeyId": CLIENT_KEY_ID,
            "scopes": ["status", "tx"],
            "iat": NOW / 1000 - 10, "exp": NOW / 1000 + 600, "jti": "cap-jti-1"
        });
        mint_jws(&header, &payload, &idp_key())
    }

    /// A valid client-signed grant whose `aud` is `agent_key_id`, carrying the
    /// capabilityToken inside (v1 convention).
    fn grant_for_agent(agent_key_id: &str) -> Value {
        let mut grant = json!({
            "type": "txArmGrant",
            "aud": agent_key_id,
            "clientKeyId": CLIENT_KEY_ID,
            "sessionId": "sess-1",
            "capabilityJti": "cap-jti-1",
            "capabilityToken": token_for_agent(agent_key_id),
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
        let sig = client_key().sign(&canon);
        grant.insert("clientSig".to_string(), json!(b64url(&sig.to_bytes())));
        Value::Object(grant)
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
        let mut client_transport = initiator.into_transport();

        // Encrypt the Arm control frame and hand it to the session as an env.
        let arm_grant = grant_for_agent(&agent_kid);
        let plaintext = serde_json::to_vec(&arm_grant).unwrap();
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

        let mut ctx = ctx_with_agent(&agent_kid, true, true);
        with_consent(&ctx, true);
        let bus = MessageBus::new(64).unwrap();
        let d = dispatch_action(action, &mut ctx, &bus, NOW).await;
        assert_eq!(d, Dispatch::Continue);
        assert!(
            ctx.arm.lock().unwrap().tx_permitted(NOW),
            "end-to-end: a verified Arm over Noise must permit remote TX"
        );

        // Negative: the same flow with consent OFF never permits.
        let mut ctx_off = ctx_with_agent(&agent_kid, true, true);
        // (consent left OFF)
        let action2 = map_client_frame(&decrypted).unwrap();
        dispatch_action(action2, &mut ctx_off, &bus, NOW).await;
        // jti replay guard is per-ctx (fresh seen set), so this arms the state
        // machine but consent-off denies at the gate.
        assert!(
            !ctx_off.arm.lock().unwrap().tx_permitted(NOW),
            "consent OFF must deny even after a verified Arm over Noise"
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
        ArmContext {
            arm: Arc::new(Mutex::new(ArmState::new())),
            verifier: CapabilityVerifier {
                agent_key_id: agent_kid.to_string(),
                pinned_idp_keys: vec![IdpKey {
                    kid: IDP_KID.to_string(),
                    public_key: idp_key().verifying_key().to_bytes(),
                }],
            },
            client_keys,
            tx_allow_list: allow,
            seen_jtis: HashSet::new(),
            audit: AuditLog::new(audit_tmp()),
        }
    }
}
