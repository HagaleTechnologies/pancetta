//! Multi-peer agent-leg session driver: demux up to [`MAX_PEERS`] independent
//! Noise sessions over ONE relay websocket.
//!
//! [`MultiPeerSession`] is a sibling of [`crate::session::AgentSession`] (the
//! single-peer reference, kept untouched). It shares the same relay auth leg
//! (`hello` → `auth` → `ready`) but, instead of owning one
//! `ResponderHandshake`/`NoiseTransport`, it keeps a `HashMap<peerKeyId,
//! PeerState>` and runs an INDEPENDENT Noise IK handshake/transport per peer,
//! keyed by the keyId the peer PROVED in its Noise msg1 (PAN-95,
//! [`crate::peer_identity`]). The relay's `env.src` is a routing label: it is
//! the cheap pre-allocation filter, and it must equal the proven keyId.
//!
//! Security posture (hard requirements):
//! - **Peer isolation.** Each peer has its own [`NoiseTransport`] with its own
//!   keys and nonces. One peer's ciphertext is cryptographically undecryptable
//!   by another peer's transport; a decrypt failure for one peer removes only
//!   that peer and never touches any other peer's state.
//! - **Admission before allocation.** For an `env` from an unknown `src`, the
//!   allow-list membership + [`MAX_PEERS`] capacity checks run BEFORE any
//!   `ResponderHandshake` is built, so an unlisted or 9th peer never costs a
//!   handshake allocation or a reply. Admission is re-applied to established
//!   peers whenever the set is replaced ([`MultiPeerSession::replace_allowed`],
//!   PAN-96): a peer the new set no longer lists is evicted, its transport is
//!   dropped, and its next frame is refused like any unlisted `src`.
//! - **Unattributable drop.** An `env` with no `src` (the DO stamps `src` on
//!   every forward) is never trusted — it is dropped ([`Poll::Idle`]).
//! - **Identity before establishment.** A peer is established only when its
//!   msg1 carries a client identity proof whose `idSig` binds the static key
//!   the Noise handshake authenticated, and whose derived keyId equals `src`.
//!   Every check runs before msg2 is sent or any [`PeerState`] is inserted, so
//!   a relay label alone never makes a session that client's.
//!
//! The per-peer state is deliberately minimal: Noise IK completes in one round
//! trip from the responder's view, so a peer needs only its established
//! transport plus its channel-binding session id.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::keys::AgentIdentity;
use crate::noise::{NoiseTransport, ResponderHandshake};
use crate::peer_identity::{verify_client_identity_proof, PeerIdentityError};
use crate::relay::{
    decode_env_payload, encode_env_payload, is_terminal, parse_frame, RecvOutcome, RelayFrame,
    WsConn,
};
use crate::session::SessionError;

/// Maximum number of concurrent peers demuxed over one relay socket. Mirrors
/// the relay contract's `MAX_CLIENTS` — the DO will not admit more clients to a
/// single agent than this, so a 9th peer is a protocol anomaly and is refused.
pub const MAX_PEERS: usize = 8;

/// Domain-separation tag for the relay agent-auth signature (relay.v1 auth.sig,
/// Q-0011 pancetta refinement). Duplicated from `session.rs` (the plan accepts
/// this small duplication rather than sharing a helper out of the untouched
/// single-peer reference).
const AUTH_DOMAIN_TAG: &str = "cqdx-relay-agent-auth-v1";

/// Encode bytes as unpadded base64url (channel-binding session id encoding).
fn b64url_unpadded(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Per-peer session state. Minimal by construction: once the one-round-trip IK
/// handshake completes there is nothing left to hold but the transport and the
/// channel binding.
struct PeerState {
    /// This peer's INDEPENDENT Noise transport (its own keys + nonces). Never
    /// shared with any other peer — the crux of peer isolation.
    transport: NoiseTransport,
    /// Unpadded base64url of this peer's Noise handshake hash (`h`), captured
    /// the instant its handshake completed (dispensa Q-0022 channel binding).
    session_id: String,
}

/// Why a relay-forwarded peer was refused. No state is allocated and no reply
/// is sent in every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// `src` is not in the station-local admission list.
    NotAllowListed,
    /// [`MAX_PEERS`] peers are already established.
    AtCapacity,
    /// The env payload or Noise msg1 could not be processed.
    Handshake,
    /// msg1 carried no client identity proof.
    IdentityProofMissing,
    /// The proof is malformed, of an unsupported version, or its `idSig` does
    /// not bind the handshake-authenticated static key.
    IdentityProofInvalid,
    /// The proof is valid, but for a keyId other than the relay's `src` label.
    IdentityMismatch,
}

/// The outcome of a single [`MultiPeerSession::process_next`] step.
#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    /// A decrypted control-frame plaintext from an established peer.
    Plaintext {
        /// The peer keyId (proven in the peer's Noise msg1).
        peer: String,
        /// The recovered plaintext.
        plaintext: Vec<u8>,
    },
    /// A peer completed its Noise handshake; `session_id` is its channel
    /// binding (unpadded base64url of the handshake hash).
    PeerEstablished {
        /// The newly established peer keyId.
        peer: String,
        /// The peer's channel-binding session id.
        session_id: String,
    },
    /// A relay-forwarded peer was refused (see [`RefuseReason`]). No state was
    /// allocated and no reply was sent.
    PeerRefused {
        /// The relay's `src` label for the refused peer (unproven).
        peer: String,
        /// Why it was refused.
        reason: RefuseReason,
    },
    /// A peer left (presence `down`) or its transport failed; its state is gone.
    PeerDown {
        /// The removed peer keyId.
        peer: String,
    },
    /// A benign frame advanced the relay leg (ready / presence-up / transient
    /// error / unattributable env).
    Idle,
    /// Nothing arrived within the timeout.
    Quiet,
    /// The socket is closed/drained — the session is over.
    Closed,
}

/// Drives the agent side of a relay connection, demuxing many peers over one
/// [`WsConn`]. Generic over the WS seam so tests use a scripted mock.
pub struct MultiPeerSession<'a, W: WsConn> {
    ws: W,
    identity: &'a AgentIdentity,
    /// The set of client keyIds this agent will admit. An `env` from a `src`
    /// outside this set is refused before any handshake state is allocated.
    /// Refreshed via [`Self::replace_allowed`] on a live authorization refresh.
    allowed: HashSet<String>,
    /// Established (or in-transport) peers, keyed by the keyId each PROVED in
    /// its Noise msg1. Each entry owns an INDEPENDENT Noise transport.
    peers: HashMap<String, PeerState>,
}

impl<'a, W: WsConn> MultiPeerSession<'a, W> {
    /// Create a multi-peer session over `ws` for `identity`, admitting only the
    /// peers in `allowed`.
    pub fn new(ws: W, identity: &'a AgentIdentity, allowed: HashSet<String>) -> Self {
        Self {
            ws,
            identity,
            allowed,
            peers: HashMap::new(),
        }
    }

    /// Recv `hello`, then send `auth`. A byte-for-byte copy of
    /// `AgentSession::authenticate` — the plan keeps the single-peer reference
    /// untouched, so this leg is duplicated rather than shared.
    pub fn authenticate(&mut self) -> Result<(), SessionError> {
        let text = self
            .ws
            .recv_text()?
            .ok_or(SessionError::UnexpectedClose("awaiting hello"))?;
        let frame = parse_frame(&text)?;
        let challenge_b64 = match frame {
            RelayFrame::Hello { challenge } => challenge,
            RelayFrame::Error { code, .. } => return self.handle_error_frame(code),
            other => {
                return Err(SessionError::UnexpectedFrame {
                    phase: "awaiting hello",
                    got: describe(&other),
                })
            }
        };
        let challenge = decode_env_payload(&challenge_b64)
            .map_err(|e| SessionError::BadChallenge(e.to_string()))?;
        let sig = self.identity.sign_domain(AUTH_DOMAIN_TAG, &challenge);
        let auth = RelayFrame::Auth {
            role: "agent".to_string(),
            agent_key_id: self.identity.key_id(),
            sig: encode_env_payload(&sig),
        };
        self.ws.send_text(auth.to_json()?)?;
        Ok(())
    }

    /// Process a single inbound frame (bounded by `timeout`), demuxing to the
    /// right peer. See [`Poll`] for the outcomes.
    pub fn process_next(&mut self, timeout: Duration) -> Result<Poll, SessionError> {
        let text = match self.ws.recv_text_within(timeout)? {
            RecvOutcome::Frame(t) => t,
            RecvOutcome::Quiet => return Ok(Poll::Quiet),
            RecvOutcome::Closed => return Ok(Poll::Closed),
        };
        let frame = parse_frame(&text)?;
        match frame {
            RelayFrame::Ready { .. } => Ok(Poll::Idle),
            RelayFrame::Presence { peer, state } => {
                if state == "down" && self.peers.remove(&peer).is_some() {
                    Ok(Poll::PeerDown { peer })
                } else {
                    Ok(Poll::Idle)
                }
            }
            RelayFrame::Env { payload, src, .. } => self.process_env(&payload, src),
            RelayFrame::Error { code, .. } => self.handle_error_frame(code).map(|()| Poll::Idle),
            RelayFrame::Bye { .. } => Err(SessionError::UnexpectedClose("peer sent bye")),
            other @ (RelayFrame::Hello { .. } | RelayFrame::Auth { .. }) => {
                Err(SessionError::UnexpectedFrame {
                    phase: "post-auth",
                    got: describe(&other),
                })
            }
        }
    }

    /// Demux one `env` to its peer: unattributable → drop; known peer →
    /// per-peer transport decrypt (failure removes only that peer); unknown
    /// peer → admission check BEFORE any allocation, then handshake bootstrap.
    fn process_env(
        &mut self,
        payload_b64: &str,
        src: Option<String>,
    ) -> Result<Poll, SessionError> {
        // The DO stamps an authenticated `src` on EVERY forwarded env; an env
        // with no src is unattributable and never trusted.
        let peer = match src {
            None => return Ok(Poll::Idle),
            Some(s) => s,
        };

        if self.peers.contains_key(&peer) {
            // Established peer: decode + decrypt under ITS OWN transport. Any
            // failure (bad base64 or bad ciphertext) is a per-peer fault — we
            // remove ONLY this peer and never touch another peer's state, and
            // never surface it as a whole-session error.
            let payload = match decode_env_payload(payload_b64) {
                Ok(p) => p,
                Err(_) => {
                    self.peers.remove(&peer);
                    return Ok(Poll::PeerDown { peer });
                }
            };
            let state = self
                .peers
                .get_mut(&peer)
                .expect("peer present (checked above)");
            match state.transport.decrypt(&payload) {
                Ok(plaintext) => Ok(Poll::Plaintext { peer, plaintext }),
                Err(_) => {
                    self.peers.remove(&peer);
                    Ok(Poll::PeerDown { peer })
                }
            }
        } else {
            // Unknown peer. Admission FIRST — allow-list membership + capacity
            // on the relay's `src` label — so an unlisted or 9th peer never
            // costs a ResponderHandshake or a reply.
            if !self.allowed.contains(&peer) {
                return Ok(Poll::PeerRefused {
                    peer,
                    reason: RefuseReason::NotAllowListed,
                });
            }
            if self.peers.len() >= MAX_PEERS {
                return Ok(Poll::PeerRefused {
                    peer,
                    reason: RefuseReason::AtCapacity,
                });
            }
            // Only now decode + run the handshake bootstrap. Any failure leaves
            // NO state behind (nothing is inserted until the handshake and the
            // identity proof fully succeed) and refuses the peer.
            let msg1 = match decode_env_payload(payload_b64) {
                Ok(p) => p,
                Err(_) => {
                    return Ok(Poll::PeerRefused {
                        peer,
                        reason: RefuseReason::Handshake,
                    })
                }
            };
            match self.bootstrap_peer(&peer, &msg1) {
                Ok(session_id) => Ok(Poll::PeerEstablished { peer, session_id }),
                Err(reason) => Ok(Poll::PeerRefused { peer, reason }),
            }
        }
    }

    /// Run the responder Noise IK bootstrap for a freshly admitted `src`:
    /// `read_msg1` → verify the client identity proof against the
    /// handshake-authenticated static key → require the proven keyId to equal
    /// `src` → `write_msg2(&[])` → reply `env{dst}` → capture the channel
    /// binding → `into_transport`, inserting the [`PeerState`] (keyed by the
    /// PROVEN keyId) only on full success. Nothing is sent and nothing is
    /// inserted before the identity checks pass.
    fn bootstrap_peer(&mut self, src: &str, msg1: &[u8]) -> Result<String, RefuseReason> {
        let priv_bytes = self.identity.agreement_private_bytes();
        let mut hs = ResponderHandshake::new(&priv_bytes).map_err(|_| RefuseReason::Handshake)?;
        let payload = hs.read_msg1(msg1).map_err(|_| RefuseReason::Handshake)?;
        let remote_static = hs.remote_static().ok_or(RefuseReason::Handshake)?;
        // Identity BEFORE any reply or state: the peer is whoever the proof proves.
        let proven =
            verify_client_identity_proof(&payload, &remote_static).map_err(|e| match e {
                PeerIdentityError::Missing => RefuseReason::IdentityProofMissing,
                _ => RefuseReason::IdentityProofInvalid,
            })?;
        // `src` is routing metadata: it must name the proven identity.
        if proven != src {
            return Err(RefuseReason::IdentityMismatch);
        }
        let msg2 = hs.write_msg2(&[]).map_err(|_| RefuseReason::Handshake)?;
        let out = RelayFrame::Env {
            dst: proven.clone(),
            payload: encode_env_payload(&msg2),
            src: None,
        };
        let json = out.to_json().map_err(|_| RefuseReason::Handshake)?;
        self.ws
            .send_text(json)
            .map_err(|_| RefuseReason::Handshake)?;
        // Capture the handshake hash BEFORE `into_transport` consumes `hs`
        // (Q-0022 channel binding — available only pre-transport).
        let session_id = b64url_unpadded(&hs.handshake_hash());
        let transport = hs.into_transport().map_err(|_| RefuseReason::Handshake)?;
        self.peers.insert(
            proven,
            PeerState {
                transport,
                session_id: session_id.clone(),
            },
        );
        Ok(session_id)
    }

    /// Encrypt `plaintext` under `peer`'s transport and send it as an `env`.
    /// Errors if `peer` is not an established peer.
    pub fn send_to(&mut self, peer: &str, plaintext: &[u8]) -> Result<(), SessionError> {
        // Scope the peers borrow to just the encrypt so `self.ws` is free after.
        let ct = {
            let state = self
                .peers
                .get_mut(peer)
                .ok_or_else(|| SessionError::UnexpectedFrame {
                    phase: "send_to",
                    got: format!("unknown peer {peer}"),
                })?;
            state.transport.encrypt(plaintext)?
        };
        let env = RelayFrame::Env {
            dst: peer.to_string(),
            payload: encode_env_payload(&ct),
            src: None,
        };
        self.ws.send_text(env.to_json()?)?;
        Ok(())
    }

    /// Encrypt `plaintext` once per established peer for which `recipient`
    /// returns `true` — each under its OWN transport keys/nonces, so no
    /// ciphertext is ever reused across peers — and send. The caller decides
    /// who is authorized to receive; this type knows nothing about scopes, and
    /// there is deliberately no unfiltered fan-out (PAN-95). Best-effort: a
    /// peer whose encrypt/send fails is removed and skipped (never torn down as
    /// a session error). Returns the number of successful sends.
    pub fn broadcast_to<F: FnMut(&str) -> bool>(
        &mut self,
        plaintext: &[u8],
        mut recipient: F,
    ) -> usize {
        // Phase 1: encrypt per selected peer (needs &mut on each transport).
        // Collect the ready-to-send env JSON so the peers borrow is released
        // before we touch `self.ws`.
        let mut outbound: Vec<(String, String)> = Vec::with_capacity(self.peers.len());
        let mut failed: Vec<String> = Vec::new();
        for (peer, state) in self.peers.iter_mut() {
            if !recipient(peer) {
                continue;
            }
            match state.transport.encrypt(plaintext) {
                Ok(ct) => {
                    let env = RelayFrame::Env {
                        dst: peer.clone(),
                        payload: encode_env_payload(&ct),
                        src: None,
                    };
                    match env.to_json() {
                        Ok(json) => outbound.push((peer.clone(), json)),
                        Err(_) => failed.push(peer.clone()),
                    }
                }
                Err(_) => failed.push(peer.clone()),
            }
        }
        // Phase 2: send.
        let mut sent = 0;
        for (peer, json) in outbound {
            match self.ws.send_text(json) {
                Ok(()) => sent += 1,
                Err(_) => failed.push(peer),
            }
        }
        // Remove any peer that failed to encrypt or send.
        for p in failed {
            self.peers.remove(&p);
        }
        sent
    }

    /// The channel-binding session id for `peer`, if established.
    pub fn session_id(&self, peer: &str) -> Option<&str> {
        self.peers.get(peer).map(|s| s.session_id.as_str())
    }

    /// Iterate the keyIds of currently established peers.
    pub fn established_peers(&self) -> impl Iterator<Item = &str> {
        self.peers.keys().map(|k| k.as_str())
    }

    /// Replace the admission set (a live authorization refresh) and evict every
    /// established peer it no longer contains, dropping that peer's transport: its
    /// next frame is treated as an unknown `src` and refused `NotAllowListed`, and
    /// re-admission needs a fresh handshake. Newly listed peers may now establish.
    /// Returns the evicted keyIds, sorted.
    pub fn replace_allowed(&mut self, allowed: HashSet<String>) -> Vec<String> {
        let mut evicted: Vec<String> = self
            .peers
            .keys()
            .filter(|p| !allowed.contains(*p))
            .cloned()
            .collect();
        for p in &evicted {
            self.peers.remove(p);
        }
        evicted.sort();
        self.allowed = allowed;
        evicted
    }

    /// Map an `error` frame to keep-waiting (transient) or stop (terminal).
    /// Duplicated from `AgentSession` (untouched single-peer reference).
    fn handle_error_frame(&self, code: String) -> Result<(), SessionError> {
        if is_terminal(&code) {
            Err(SessionError::Terminal { code })
        } else {
            Ok(())
        }
    }
}

/// A short human description of a frame for error messages (no secret content).
/// Duplicated from `session.rs` (the untouched single-peer reference).
fn describe(f: &RelayFrame) -> String {
    match f {
        RelayFrame::Hello { .. } => "hello".into(),
        RelayFrame::Auth { .. } => "auth".into(),
        RelayFrame::Ready { .. } => "ready".into(),
        RelayFrame::Presence { .. } => "presence".into(),
        RelayFrame::Env { .. } => "env".into(),
        RelayFrame::Bye { .. } => "bye".into(),
        RelayFrame::Error { code, .. } => format!("error({code})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::IDSIG_TAG;
    use crate::peer_identity::encode_client_identity_proof;
    use base64::Engine as _;

    const TIMEOUT: Duration = Duration::from_millis(1);

    fn b64url(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }
    fn unb64url(s: &str) -> Vec<u8> {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s)
            .unwrap()
    }

    /// A scripted mock WS: a queue of inbound frames + a capture of outbound.
    struct MockWs {
        inbound: std::collections::VecDeque<String>,
        outbound: Vec<String>,
    }
    impl MockWs {
        fn new(inbound: Vec<String>) -> Self {
            Self {
                inbound: inbound.into_iter().collect(),
                outbound: Vec::new(),
            }
        }
        fn push_inbound(&mut self, s: String) {
            self.inbound.push_back(s);
        }
    }
    impl WsConn for MockWs {
        fn send_text(&mut self, s: String) -> Result<(), crate::relay::RelayError> {
            self.outbound.push(s);
            Ok(())
        }
        fn recv_text(&mut self) -> Result<Option<String>, crate::relay::RelayError> {
            Ok(self.inbound.pop_front())
        }
    }

    /// A test-only Noise IK initiator (the client side).
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
        fn read_msg2(&mut self, msg2: &[u8]) -> Vec<u8> {
            let mut buf = vec![0u8; 65535];
            let n = self.inner.read_message(msg2, &mut buf).unwrap();
            buf.truncate(n);
            buf
        }
        fn into_transport(self) -> snow::TransportState {
            self.inner.into_transport_mode().unwrap()
        }
        fn handshake_hash(&self) -> Vec<u8> {
            self.inner.get_handshake_hash().to_vec()
        }
    }

    /// Encrypt with a raw snow transport (client side).
    fn ts_encrypt(t: &mut snow::TransportState, msg: &[u8]) -> Vec<u8> {
        let mut ct = vec![0u8; msg.len() + 16 + 1];
        let n = t.write_message(msg, &mut ct).unwrap();
        ct.truncate(n);
        ct
    }
    /// Attempt to decrypt with a raw snow transport (client side).
    fn ts_decrypt(t: &mut snow::TransportState, ct: &[u8]) -> Result<Vec<u8>, snow::Error> {
        let mut buf = vec![0u8; ct.len().max(1)];
        let n = t.read_message(ct, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn env_json(dst: &str, payload: &[u8], src: Option<&str>) -> String {
        RelayFrame::Env {
            dst: dst.to_string(),
            payload: b64url(payload),
            src: src.map(|s| s.to_string()),
        }
        .to_json()
        .unwrap()
    }

    /// Drive the shared relay auth leg to admitted (hello → auth → ready).
    fn admit(identity: &AgentIdentity, allowed: HashSet<String>) -> MultiPeerSession<'_, MockWs> {
        let hello = RelayFrame::Hello {
            challenge: b64url(&[9u8; 32]),
        }
        .to_json()
        .unwrap();
        let ready = RelayFrame::Ready {
            key_id: identity.key_id(),
            peer_present: false,
        }
        .to_json()
        .unwrap();
        let ws = MockWs::new(vec![hello, ready]);
        let mut sess = MultiPeerSession::new(ws, identity, allowed);
        sess.authenticate().unwrap();
        assert_eq!(sess.process_next(TIMEOUT).unwrap(), Poll::Idle); // ready
        sess
    }

    /// A client device with a real pairing.v1 identity (Ed25519 + X25519).
    struct TestDevice {
        id: AgentIdentity,
    }
    impl TestDevice {
        fn new(seed: u8) -> Self {
            Self {
                id: AgentIdentity::from_secret_bytes([seed; 32], [seed.wrapping_add(1); 32]),
            }
        }
        /// The device's self-certifying keyId.
        fn kid(&self) -> String {
            self.id.key_id()
        }
        /// A Noise initiator using the device's enrolled agreement key.
        fn initiator(&self, agent_pub: &[u8]) -> TestInitiator {
            TestInitiator::new(&self.id.agreement_private_bytes(), agent_pub)
        }
        /// The device's msg1 identity proof (its enrollment idSig, reused).
        fn proof(&self) -> Vec<u8> {
            let id_sig = self
                .id
                .sign_domain(IDSIG_TAG, &self.id.agreement_public_raw());
            encode_client_identity_proof(&self.id.identity_public_raw(), &id_sig)
        }
    }

    fn allow(devices: &[&TestDevice]) -> HashSet<String> {
        devices.iter().map(|d| d.kid()).collect()
    }

    /// Push a device's msg1 (with its identity proof, `src` = its keyId), run
    /// one step, assert PeerEstablished, feed msg2 back to the initiator, and
    /// return the agent-reported session_id.
    fn bootstrap(
        sess: &mut MultiPeerSession<'_, MockWs>,
        initiator: &mut TestInitiator,
        device: &TestDevice,
        agent_key_id: &str,
    ) -> String {
        let peer_id = device.kid();
        let msg1 = initiator.write_msg1(&device.proof());
        sess.ws
            .push_inbound(env_json(agent_key_id, &msg1, Some(&peer_id)));
        let sid = match sess.process_next(TIMEOUT).unwrap() {
            Poll::PeerEstablished { peer, session_id } => {
                assert_eq!(peer, peer_id, "established the expected peer");
                session_id
            }
            other => panic!("expected PeerEstablished for {peer_id}, got {other:?}"),
        };
        // The msg2 env just emitted must be addressed to this peer.
        let msg2 = match parse_frame(sess.ws.outbound.last().unwrap()).unwrap() {
            RelayFrame::Env { dst, payload, src } => {
                assert_eq!(dst, peer_id, "msg2 addressed to the right peer");
                assert!(src.is_none(), "agent never sets src");
                unb64url(&payload)
            }
            _ => panic!("expected env msg2"),
        };
        initiator.read_msg2(&msg2);
        sid
    }

    /// Push a raw msg1 labelled `src`, run one step, and assert it was refused
    /// for `want` with no reply and no state.
    fn assert_refused(
        sess: &mut MultiPeerSession<'_, MockWs>,
        msg1: &[u8],
        src: &str,
        agent_key_id: &str,
        want: RefuseReason,
    ) {
        let outbound_before = sess.ws.outbound.len();
        let established_before = sess.established_peers().count();
        sess.ws
            .push_inbound(env_json(agent_key_id, msg1, Some(src)));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::PeerRefused {
                peer: src.to_string(),
                reason: want,
            }
        );
        assert_eq!(
            sess.ws.outbound.len(),
            outbound_before,
            "refused peer costs no reply (no msg2)"
        );
        assert!(sess.session_id(src).is_none(), "refused peer has no state");
        assert_eq!(
            sess.established_peers().count(),
            established_before,
            "refusal allocates no peer"
        );
    }

    #[test]
    fn two_peers_handshake_and_route_independently() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let (dev_a, dev_b) = (TestDevice::new(0xAA), TestDevice::new(0xBB));
        let (kid_a, kid_b) = (dev_a.kid(), dev_b.kid());
        let mut sess = admit(&identity, allow(&[&dev_a, &dev_b]));

        let mut a = dev_a.initiator(&agent_pub);
        let mut b = dev_b.initiator(&agent_pub);

        let sid_a = bootstrap(&mut sess, &mut a, &dev_a, &identity.key_id());
        let a_hash = a.handshake_hash();
        let sid_b = bootstrap(&mut sess, &mut b, &dev_b, &identity.key_id());
        let b_hash = b.handshake_hash();

        // Two DISTINCT channel bindings — no conflation.
        assert_ne!(sid_a, sid_b, "each peer has an independent session_id");
        assert_eq!(sid_a, b64url(&a_hash), "sid_a is A's handshake hash");
        assert_eq!(sid_b, b64url(&b_hash), "sid_b is B's handshake hash");
        assert_eq!(sess.session_id(&kid_a), Some(sid_a.as_str()));
        assert_eq!(sess.session_id(&kid_b), Some(sid_b.as_str()));

        let mut ta = a.into_transport();
        let mut tb = b.into_transport();

        // Transport traffic in ALTERNATING order, each routed to the right peer
        // with the exact bytes — no cross-talk.
        let ct_a = ts_encrypt(&mut ta, b"from-a-1");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct_a, Some(&kid_a)));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::Plaintext {
                peer: kid_a.clone(),
                plaintext: b"from-a-1".to_vec()
            }
        );

        let ct_b = ts_encrypt(&mut tb, b"from-b-1");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct_b, Some(&kid_b)));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::Plaintext {
                peer: kid_b.clone(),
                plaintext: b"from-b-1".to_vec()
            }
        );

        let ct_a2 = ts_encrypt(&mut ta, b"from-a-2");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct_a2, Some(&kid_a)));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::Plaintext {
                peer: kid_a.clone(),
                plaintext: b"from-a-2".to_vec()
            }
        );
    }

    #[test]
    fn unlisted_peer_is_refused_without_state() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let dev_a = TestDevice::new(0xAA);
        let mallory = TestDevice::new(0xCC);
        let mut sess = admit(&identity, allow(&[&dev_a]));

        // Even a VALID identity proof does not get an unlisted keyId in.
        let msg1 = mallory.initiator(&agent_pub).write_msg1(&mallory.proof());
        assert_refused(
            &mut sess,
            &msg1,
            &mallory.kid(),
            &identity.key_id(),
            RefuseReason::NotAllowListed,
        );
        assert!(
            !sess.established_peers().any(|p| p == mallory.kid()),
            "refused peer is not established"
        );

        // A follow-up env from mallory refuses again (still no state). Refusal
        // is decided on admission before the payload is even decoded, so the
        // relay re-forwarding the same bytes is enough to exercise it.
        assert_refused(
            &mut sess,
            &msg1,
            &mallory.kid(),
            &identity.key_id(),
            RefuseReason::NotAllowListed,
        );
    }

    #[test]
    fn ninth_peer_refused_at_capacity() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        // 9 allow-listed devices — allow-list is NOT the limiter here; capacity is.
        let devices: Vec<TestDevice> = (0..9u8).map(|i| TestDevice::new(0x10 + 2 * i)).collect();
        let allowed: HashSet<String> = devices.iter().map(|d| d.kid()).collect();
        let mut sess = admit(&identity, allowed);

        // Establish exactly MAX_PEERS (8).
        for dev in devices.iter().take(MAX_PEERS) {
            let mut init = dev.initiator(&agent_pub);
            bootstrap(&mut sess, &mut init, dev, &identity.key_id());
        }
        assert_eq!(sess.established_peers().count(), MAX_PEERS);

        // The 9th allow-listed peer's msg1 is refused at capacity.
        let ninth = &devices[MAX_PEERS];
        let msg1 = ninth.initiator(&agent_pub).write_msg1(&ninth.proof());
        assert_refused(
            &mut sess,
            &msg1,
            &ninth.kid(),
            &identity.key_id(),
            RefuseReason::AtCapacity,
        );
        assert_eq!(sess.established_peers().count(), MAX_PEERS);
    }

    /// Gherkin Scenario 1: a relay labels an envelope with an admitted client's
    /// id, uses ITS OWN initiator key, and replays the client's genuine proof.
    #[test]
    fn relay_key_claiming_admitted_client_is_refused() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let client = TestDevice::new(0x22);
        let relay = TestDevice::new(0xEE);
        let mut sess = admit(&identity, allow(&[&client]));

        // Noise authenticates the RELAY's static key; the client's idSig does
        // not cover it.
        let msg1 = relay.initiator(&agent_pub).write_msg1(&client.proof());
        assert_refused(
            &mut sess,
            &msg1,
            &client.kid(),
            &identity.key_id(),
            RefuseReason::IdentityProofInvalid,
        );
        assert!(sess.session_id(&client.kid()).is_none());
        assert_eq!(sess.established_peers().count(), 0);
    }

    #[test]
    fn relay_key_with_its_own_proof_under_client_label_is_refused() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let client = TestDevice::new(0x22);
        let relay = TestDevice::new(0xEE);
        let mut sess = admit(&identity, allow(&[&client]));

        // A valid proof — but of the relay's own identity, not the label's.
        let msg1 = relay.initiator(&agent_pub).write_msg1(&relay.proof());
        assert_refused(
            &mut sess,
            &msg1,
            &client.kid(),
            &identity.key_id(),
            RefuseReason::IdentityMismatch,
        );
        assert!(sess.session_id(&relay.kid()).is_none());
        assert_eq!(sess.established_peers().count(), 0);
    }

    #[test]
    fn missing_identity_proof_is_refused_without_reply() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let client = TestDevice::new(0x22);
        let mut sess = admit(&identity, allow(&[&client]));

        // Genuine client key, but an empty msg1 payload (a pre-PAN-95 client).
        let msg1 = client.initiator(&agent_pub).write_msg1(b"");
        assert_refused(
            &mut sess,
            &msg1,
            &client.kid(),
            &identity.key_id(),
            RefuseReason::IdentityProofMissing,
        );
    }

    #[test]
    fn genuine_client_is_established_under_its_proven_key_id() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let client = TestDevice::new(0x22);
        let mut sess = admit(&identity, allow(&[&client]));

        let mut init = client.initiator(&agent_pub);
        bootstrap(&mut sess, &mut init, &client, &identity.key_id());
        assert_eq!(
            sess.established_peers().collect::<Vec<_>>(),
            vec![client.kid().as_str()]
        );

        // Transport round-trips in both directions.
        let mut t = init.into_transport();
        let ct = ts_encrypt(&mut t, b"ping");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct, Some(&client.kid())));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::Plaintext {
                peer: client.kid(),
                plaintext: b"ping".to_vec()
            }
        );
        sess.send_to(&client.kid(), b"pong").unwrap();
        let ct = match parse_frame(sess.ws.outbound.last().unwrap()).unwrap() {
            RelayFrame::Env { dst, payload, .. } => {
                assert_eq!(dst, client.kid());
                unb64url(&payload)
            }
            _ => panic!("expected env"),
        };
        assert_eq!(ts_decrypt(&mut t, &ct).unwrap(), b"pong");
    }

    #[test]
    fn refused_impersonation_does_not_disturb_an_established_peer() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let (dev_a, dev_b) = (TestDevice::new(0xAA), TestDevice::new(0xBB));
        let relay = TestDevice::new(0xEE);
        let mut sess = admit(&identity, allow(&[&dev_a, &dev_b]));

        let mut a = dev_a.initiator(&agent_pub);
        let sid_a = bootstrap(&mut sess, &mut a, &dev_a, &identity.key_id());
        let mut ta = a.into_transport();

        // Relay attempt labelled B, with the relay's key and B's replayed proof.
        let msg1 = relay.initiator(&agent_pub).write_msg1(&dev_b.proof());
        assert_refused(
            &mut sess,
            &msg1,
            &dev_b.kid(),
            &identity.key_id(),
            RefuseReason::IdentityProofInvalid,
        );

        // A is untouched and still decrypts.
        assert_eq!(sess.session_id(&dev_a.kid()), Some(sid_a.as_str()));
        let ct = ts_encrypt(&mut ta, b"still-a");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct, Some(&dev_a.kid())));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::Plaintext {
                peer: dev_a.kid(),
                plaintext: b"still-a".to_vec()
            }
        );
    }

    #[test]
    fn presence_down_removes_only_that_peer() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let (dev_a, dev_b) = (TestDevice::new(0xAA), TestDevice::new(0xBB));
        let (kid_a, kid_b) = (dev_a.kid(), dev_b.kid());
        let mut sess = admit(&identity, allow(&[&dev_a, &dev_b]));

        let mut a = dev_a.initiator(&agent_pub);
        let mut b = dev_b.initiator(&agent_pub);
        bootstrap(&mut sess, &mut a, &dev_a, &identity.key_id());
        bootstrap(&mut sess, &mut b, &dev_b, &identity.key_id());
        let mut ta = a.into_transport();
        let mut tb = b.into_transport();

        // presence down for A → PeerDown(A); B untouched.
        let presence = RelayFrame::Presence {
            peer: kid_a.clone(),
            state: "down".to_string(),
        }
        .to_json()
        .unwrap();
        sess.ws.push_inbound(presence);
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::PeerDown {
                peer: kid_a.clone()
            }
        );
        assert!(sess.session_id(&kid_a).is_none(), "A's state is gone");
        assert!(sess.session_id(&kid_b).is_some(), "B is retained");

        // B still decrypts fine.
        let ct_b = ts_encrypt(&mut tb, b"still-here");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct_b, Some(&kid_b)));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::Plaintext {
                peer: kid_b.clone(),
                plaintext: b"still-here".to_vec()
            }
        );
        let _ = &mut ta; // A's old transport is now dead on the agent side.

        // A new env from A (fresh msg1) re-establishes A.
        let mut a2 = dev_a.initiator(&agent_pub);
        let sid = bootstrap(&mut sess, &mut a2, &dev_a, &identity.key_id());
        assert_eq!(sess.session_id(&kid_a), Some(sid.as_str()));
    }

    #[test]
    fn broadcast_to_reaches_only_selected_peers() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let (dev_a, dev_b) = (TestDevice::new(0xAA), TestDevice::new(0xBB));
        let (kid_a, kid_b) = (dev_a.kid(), dev_b.kid());
        let mut sess = admit(&identity, allow(&[&dev_a, &dev_b]));

        let mut a = dev_a.initiator(&agent_pub);
        let mut b = dev_b.initiator(&agent_pub);
        bootstrap(&mut sess, &mut a, &dev_a, &identity.key_id());
        bootstrap(&mut sess, &mut b, &dev_b, &identity.key_id());
        let mut ta = a.into_transport();
        let mut tb = b.into_transport();

        let before = sess.ws.outbound.len();
        assert_eq!(
            sess.broadcast_to(b"evt", |p| p == kid_a),
            1,
            "reached only the selected peer"
        );
        // Exactly one env, addressed to A.
        let new: Vec<RelayFrame> = sess.ws.outbound[before..]
            .iter()
            .map(|s| parse_frame(s).unwrap())
            .collect();
        assert_eq!(new.len(), 1, "one env, for the selected peer only");
        let ct_a = match &new[0] {
            RelayFrame::Env { dst, payload, src } => {
                assert!(src.is_none());
                assert_eq!(dst, &kid_a, "the only env is A's");
                assert_ne!(dst, &kid_b);
                unb64url(payload)
            }
            other => panic!("expected env, got {other:?}"),
        };

        // A decrypts ITS copy to b"evt".
        assert_eq!(ts_decrypt(&mut ta, &ct_a).unwrap(), b"evt");

        // Cross-decrypt is impossible: B's transport CANNOT read the ciphertext
        // the agent produced for A — the two transports have independent
        // keys/nonces, so A's copy is opaque to B.
        assert!(
            ts_decrypt(&mut tb, &ct_a).is_err(),
            "B's transport must NOT decrypt A's broadcast ciphertext"
        );

        // Selecting both reaches both, each under its own keys.
        let before = sess.ws.outbound.len();
        assert_eq!(sess.broadcast_to(b"evt2", |_| true), 2);
        assert_eq!(sess.ws.outbound.len(), before + 2);
    }

    #[test]
    fn garbage_transport_frame_drops_only_that_peer() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let (dev_a, dev_b) = (TestDevice::new(0xAA), TestDevice::new(0xBB));
        let (kid_a, kid_b) = (dev_a.kid(), dev_b.kid());
        let mut sess = admit(&identity, allow(&[&dev_a, &dev_b]));

        let mut a = dev_a.initiator(&agent_pub);
        let mut b = dev_b.initiator(&agent_pub);
        bootstrap(&mut sess, &mut a, &dev_a, &identity.key_id());
        bootstrap(&mut sess, &mut b, &dev_b, &identity.key_id());
        let _ta = a.into_transport();
        let mut tb = b.into_transport();

        // A valid-base64 junk payload from A fails A's transport decrypt →
        // PeerDown(A), a per-peer failure (NOT a session error).
        sess.ws.push_inbound(env_json(
            &identity.key_id(),
            b"this-is-not-a-valid-noise-frame",
            Some(&kid_a),
        ));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::PeerDown {
                peer: kid_a.clone()
            }
        );
        assert!(sess.session_id(&kid_a).is_none(), "A removed");
        assert!(sess.session_id(&kid_b).is_some(), "B untouched");

        // B still decrypts fine — its state was never perturbed.
        let ct_b = ts_encrypt(&mut tb, b"unaffected");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct_b, Some(&kid_b)));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::Plaintext {
                peer: kid_b.clone(),
                plaintext: b"unaffected".to_vec()
            }
        );
    }

    #[test]
    fn replace_allowed_evicts_established_peers_no_longer_admitted() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let (dev_a, dev_b) = (TestDevice::new(0xAA), TestDevice::new(0xBB));
        let (kid_a, kid_b) = (dev_a.kid(), dev_b.kid());
        let mut sess = admit(&identity, allow(&[&dev_a, &dev_b]));

        let mut a = dev_a.initiator(&agent_pub);
        let mut b = dev_b.initiator(&agent_pub);
        bootstrap(&mut sess, &mut a, &dev_a, &identity.key_id());
        bootstrap(&mut sess, &mut b, &dev_b, &identity.key_id());
        let mut ta = a.into_transport();
        let mut tb = b.into_transport();

        // A live authorization refresh drops B: B is evicted, A stays.
        assert_eq!(sess.replace_allowed(allow(&[&dev_a])), vec![kid_b.clone()]);
        let established: Vec<&str> = sess.established_peers().collect();
        assert_eq!(
            established,
            vec![kid_a.as_str()],
            "only A stays established"
        );
        assert!(sess.session_id(&kid_b).is_none(), "B's state is gone");

        // B's next frame, still under its old client transport, is now an
        // unknown `src` that is no longer admitted: refused, no reply.
        let outbound_before = sess.ws.outbound.len();
        let ct_b = ts_encrypt(&mut tb, b"still-here");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct_b, Some(&kid_b)));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::PeerRefused {
                peer: kid_b.clone(),
                reason: RefuseReason::NotAllowListed,
            }
        );
        assert_eq!(
            sess.ws.outbound.len(),
            outbound_before,
            "an evicted peer's frame costs no reply"
        );

        // A is untouched.
        let ct_a = ts_encrypt(&mut ta, b"from-a");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct_a, Some(&kid_a)));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::Plaintext {
                peer: kid_a.clone(),
                plaintext: b"from-a".to_vec()
            }
        );
    }

    #[test]
    fn replace_allowed_admits_newly_listed_peer_without_reconnect() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let (dev_a, dev_c) = (TestDevice::new(0xAA), TestDevice::new(0xCC));
        let mut sess = admit(&identity, allow(&[&dev_a]));

        // C is not listed yet: refused before any handshake state.
        let mut c_refused = dev_c.initiator(&agent_pub);
        let msg1 = c_refused.write_msg1(&dev_c.proof());
        assert_refused(
            &mut sess,
            &msg1,
            &dev_c.kid(),
            &identity.key_id(),
            RefuseReason::NotAllowListed,
        );

        // A refresh that adds C evicts nobody and admits C mid-session.
        assert!(sess.replace_allowed(allow(&[&dev_a, &dev_c])).is_empty());
        let mut c = dev_c.initiator(&agent_pub);
        bootstrap(&mut sess, &mut c, &dev_c, &identity.key_id());
        assert!(sess.session_id(&dev_c.kid()).is_some(), "C established");
    }

    #[test]
    fn replace_allowed_with_unchanged_set_evicts_nothing() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let (dev_a, dev_b) = (TestDevice::new(0xAA), TestDevice::new(0xBB));
        let mut sess = admit(&identity, allow(&[&dev_a, &dev_b]));

        let mut a = dev_a.initiator(&agent_pub);
        let mut b = dev_b.initiator(&agent_pub);
        bootstrap(&mut sess, &mut a, &dev_a, &identity.key_id());
        bootstrap(&mut sess, &mut b, &dev_b, &identity.key_id());

        assert!(sess.replace_allowed(allow(&[&dev_a, &dev_b])).is_empty());
        assert!(
            sess.session_id(&dev_a.kid()).is_some(),
            "A still established"
        );
        assert!(
            sess.session_id(&dev_b.kid()).is_some(),
            "B still established"
        );
        assert_eq!(sess.established_peers().count(), 2);
    }
}
