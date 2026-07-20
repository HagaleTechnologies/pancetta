//! Multi-peer agent-leg session driver: demux up to [`MAX_PEERS`] independent
//! Noise sessions over ONE relay websocket.
//!
//! [`MultiPeerSession`] is a sibling of [`crate::session::AgentSession`] (the
//! single-peer reference, kept untouched). It shares the same relay auth leg
//! (`hello` → `auth` → `ready`) but, instead of owning one
//! `ResponderHandshake`/`NoiseTransport`, it keeps a `HashMap<peerKeyId,
//! PeerState>` and runs an INDEPENDENT Noise IK handshake/transport per peer,
//! keyed by the relay-DO-authenticated `env.src`.
//!
//! Security posture (hard requirements):
//! - **Peer isolation.** Each peer has its own [`NoiseTransport`] with its own
//!   keys and nonces. One peer's ciphertext is cryptographically undecryptable
//!   by another peer's transport; a decrypt failure for one peer removes only
//!   that peer and never touches any other peer's state.
//! - **Admission before allocation.** For an `env` from an unknown `src`, the
//!   allow-list membership + [`MAX_PEERS`] capacity checks run BEFORE any
//!   `ResponderHandshake` is built, so an unlisted or 9th peer never costs a
//!   handshake allocation or a reply.
//! - **Unattributable drop.** An `env` with no `src` (the DO stamps `src` on
//!   every forward) is never trusted — it is dropped ([`Poll::Idle`]).
//!
//! The per-peer state is deliberately minimal: Noise IK completes in one round
//! trip from the responder's view, so a peer needs only its established
//! transport plus its channel-binding session id.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::keys::AgentIdentity;
use crate::noise::{NoiseTransport, ResponderHandshake};
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

/// The outcome of a single [`MultiPeerSession::process_next`] step.
#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    /// A decrypted control-frame plaintext from an established peer.
    Plaintext {
        /// The peer keyId (relay-authenticated `src`).
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
    /// A relay-admitted peer was refused (not allow-listed, or at capacity). No
    /// state was allocated and no reply was sent.
    PeerRefused {
        /// The refused peer keyId.
        peer: String,
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
    allowed: HashSet<String>,
    /// Established (or in-transport) peers, keyed by their relay-authenticated
    /// keyId. Each entry owns an INDEPENDENT Noise transport.
    peers: HashMap<String, PeerState>,
    /// Whether the relay leg has admitted us (`ready` seen). Informational.
    admitted: bool,
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
            admitted: false,
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
            RelayFrame::Ready { .. } => {
                self.admitted = true;
                Ok(Poll::Idle)
            }
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
            // Unknown peer. Admission FIRST — allow-list membership + capacity —
            // so an unlisted or 9th peer never costs a ResponderHandshake or a
            // reply.
            if !self.allowed.contains(&peer) || self.peers.len() >= MAX_PEERS {
                return Ok(Poll::PeerRefused { peer });
            }
            // Only now decode + run the handshake bootstrap. Any failure leaves
            // NO state behind (nothing is inserted until the handshake fully
            // succeeds) and refuses the peer.
            let msg1 = match decode_env_payload(payload_b64) {
                Ok(p) => p,
                Err(_) => return Ok(Poll::PeerRefused { peer }),
            };
            match self.bootstrap_peer(&peer, &msg1) {
                Ok(session_id) => Ok(Poll::PeerEstablished { peer, session_id }),
                Err(_) => Ok(Poll::PeerRefused { peer }),
            }
        }
    }

    /// Run the responder Noise IK bootstrap for a freshly admitted `peer`:
    /// `read_msg1` → `write_msg2(&[])` → reply `env{dst: peer}` → capture the
    /// channel binding → `into_transport`, inserting the [`PeerState`] only on
    /// full success. On any error nothing is inserted (no state retained).
    fn bootstrap_peer(&mut self, peer: &str, msg1: &[u8]) -> Result<String, SessionError> {
        let priv_bytes = self.identity.agreement_private_bytes();
        let mut hs = ResponderHandshake::new(&priv_bytes)?;
        hs.read_msg1(msg1)?;
        let msg2 = hs.write_msg2(&[])?;
        let out = RelayFrame::Env {
            dst: peer.to_string(),
            payload: encode_env_payload(&msg2),
            src: None,
        };
        self.ws.send_text(out.to_json()?)?;
        // Capture the handshake hash BEFORE `into_transport` consumes `hs`
        // (Q-0022 channel binding — available only pre-transport).
        let session_id = b64url_unpadded(&hs.handshake_hash());
        let transport = hs.into_transport()?;
        self.peers.insert(
            peer.to_string(),
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

    /// Encrypt `plaintext` once per established peer — each under its OWN
    /// transport keys/nonces, so no ciphertext is ever reused across peers —
    /// and send. Best-effort: a peer whose encrypt/send fails is removed and
    /// skipped (never torn down as a session error). Returns the number of
    /// successful sends.
    pub fn broadcast(&mut self, plaintext: &[u8]) -> usize {
        // Phase 1: encrypt per peer (needs &mut on each transport). Collect the
        // ready-to-send env JSON so the peers borrow is released before we
        // touch `self.ws`.
        let mut outbound: Vec<(String, String)> = Vec::with_capacity(self.peers.len());
        let mut failed: Vec<String> = Vec::new();
        for (peer, state) in self.peers.iter_mut() {
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

    /// Push a peer's msg1, run one step, assert PeerEstablished, feed msg2 back
    /// to the initiator, and return the agent-reported session_id.
    fn bootstrap(
        sess: &mut MultiPeerSession<'_, MockWs>,
        initiator: &mut TestInitiator,
        peer_id: &str,
        agent_key_id: &str,
    ) -> String {
        let msg1 = initiator.write_msg1(b"");
        sess.ws
            .push_inbound(env_json(agent_key_id, &msg1, Some(peer_id)));
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

    #[test]
    fn two_peers_handshake_and_route_independently() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let allowed: HashSet<String> = ["PEER-A", "PEER-B"].iter().map(|s| s.to_string()).collect();
        let mut sess = admit(&identity, allowed);

        let mut a = TestInitiator::new(&[0xAA; 32], &agent_pub);
        let mut b = TestInitiator::new(&[0xBB; 32], &agent_pub);

        let sid_a = bootstrap(&mut sess, &mut a, "PEER-A", &identity.key_id());
        let a_hash = a.handshake_hash();
        let sid_b = bootstrap(&mut sess, &mut b, "PEER-B", &identity.key_id());
        let b_hash = b.handshake_hash();

        // Two DISTINCT channel bindings — no conflation.
        assert_ne!(sid_a, sid_b, "each peer has an independent session_id");
        assert_eq!(sid_a, b64url(&a_hash), "sid_a is A's handshake hash");
        assert_eq!(sid_b, b64url(&b_hash), "sid_b is B's handshake hash");
        assert_eq!(sess.session_id("PEER-A"), Some(sid_a.as_str()));
        assert_eq!(sess.session_id("PEER-B"), Some(sid_b.as_str()));

        let mut ta = a.into_transport();
        let mut tb = b.into_transport();

        // Transport traffic in ALTERNATING order, each routed to the right peer
        // with the exact bytes — no cross-talk.
        let ct_a = ts_encrypt(&mut ta, b"from-a-1");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct_a, Some("PEER-A")));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::Plaintext {
                peer: "PEER-A".to_string(),
                plaintext: b"from-a-1".to_vec()
            }
        );

        let ct_b = ts_encrypt(&mut tb, b"from-b-1");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct_b, Some("PEER-B")));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::Plaintext {
                peer: "PEER-B".to_string(),
                plaintext: b"from-b-1".to_vec()
            }
        );

        let ct_a2 = ts_encrypt(&mut ta, b"from-a-2");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct_a2, Some("PEER-A")));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::Plaintext {
                peer: "PEER-A".to_string(),
                plaintext: b"from-a-2".to_vec()
            }
        );
    }

    #[test]
    fn unlisted_peer_is_refused_without_state() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let allowed: HashSet<String> = ["PEER-A"].iter().map(|s| s.to_string()).collect();
        let mut sess = admit(&identity, allowed);

        let mut mallory = TestInitiator::new(&[0xCC; 32], &agent_pub);
        let msg1 = mallory.write_msg1(b"");
        let outbound_before = sess.ws.outbound.len();
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &msg1, Some("mallory")));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::PeerRefused {
                peer: "mallory".to_string()
            }
        );
        // No new env emitted (no msg2 reply), no state allocated.
        assert_eq!(
            sess.ws.outbound.len(),
            outbound_before,
            "refused peer costs no reply"
        );
        assert!(
            !sess.established_peers().any(|p| p == "mallory"),
            "refused peer is not established"
        );
        assert!(sess.session_id("mallory").is_none());

        // A follow-up env from mallory refuses again (still no state). Refusal
        // is decided on admission before the payload is even decoded, so the
        // relay re-forwarding the same bytes is enough to exercise it.
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &msg1, Some("mallory")));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::PeerRefused {
                peer: "mallory".to_string()
            }
        );
        assert!(!sess.established_peers().any(|p| p == "mallory"));
    }

    #[test]
    fn ninth_peer_refused_at_capacity() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        // 9 allow-listed ids — allow-list is NOT the limiter here; capacity is.
        let allowed: HashSet<String> = (0..9).map(|i| format!("PEER-{i}")).collect();
        let mut sess = admit(&identity, allowed);

        // Establish exactly MAX_PEERS (8).
        for i in 0..MAX_PEERS {
            let seed = [0x10u8 + i as u8; 32];
            let mut init = TestInitiator::new(&seed, &agent_pub);
            bootstrap(
                &mut sess,
                &mut init,
                &format!("PEER-{i}"),
                &identity.key_id(),
            );
        }
        assert_eq!(sess.established_peers().count(), MAX_PEERS);

        // The 9th allow-listed peer's msg1 is refused at capacity.
        let mut ninth = TestInitiator::new(&[0x99; 32], &agent_pub);
        let msg1 = ninth.write_msg1(b"");
        let outbound_before = sess.ws.outbound.len();
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &msg1, Some("PEER-8")));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::PeerRefused {
                peer: "PEER-8".to_string()
            }
        );
        assert_eq!(
            sess.ws.outbound.len(),
            outbound_before,
            "9th peer costs no reply"
        );
        assert_eq!(sess.established_peers().count(), MAX_PEERS);
    }

    #[test]
    fn presence_down_removes_only_that_peer() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let allowed: HashSet<String> = ["PEER-A", "PEER-B"].iter().map(|s| s.to_string()).collect();
        let mut sess = admit(&identity, allowed);

        let mut a = TestInitiator::new(&[0xAA; 32], &agent_pub);
        let mut b = TestInitiator::new(&[0xBB; 32], &agent_pub);
        bootstrap(&mut sess, &mut a, "PEER-A", &identity.key_id());
        bootstrap(&mut sess, &mut b, "PEER-B", &identity.key_id());
        let mut ta = a.into_transport();
        let mut tb = b.into_transport();

        // presence down for A → PeerDown(A); B untouched.
        let presence = RelayFrame::Presence {
            peer: "PEER-A".to_string(),
            state: "down".to_string(),
        }
        .to_json()
        .unwrap();
        sess.ws.push_inbound(presence);
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::PeerDown {
                peer: "PEER-A".to_string()
            }
        );
        assert!(sess.session_id("PEER-A").is_none(), "A's state is gone");
        assert!(sess.session_id("PEER-B").is_some(), "B is retained");

        // B still decrypts fine.
        let ct_b = ts_encrypt(&mut tb, b"still-here");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct_b, Some("PEER-B")));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::Plaintext {
                peer: "PEER-B".to_string(),
                plaintext: b"still-here".to_vec()
            }
        );
        let _ = &mut ta; // A's old transport is now dead on the agent side.

        // A new env from A (fresh msg1) re-establishes A.
        let mut a2 = TestInitiator::new(&[0xAA; 32], &agent_pub);
        let sid = bootstrap(&mut sess, &mut a2, "PEER-A", &identity.key_id());
        assert_eq!(sess.session_id("PEER-A"), Some(sid.as_str()));
    }

    #[test]
    fn broadcast_reaches_each_established_peer_individually() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let allowed: HashSet<String> = ["PEER-A", "PEER-B"].iter().map(|s| s.to_string()).collect();
        let mut sess = admit(&identity, allowed);

        let mut a = TestInitiator::new(&[0xAA; 32], &agent_pub);
        let mut b = TestInitiator::new(&[0xBB; 32], &agent_pub);
        bootstrap(&mut sess, &mut a, "PEER-A", &identity.key_id());
        bootstrap(&mut sess, &mut b, "PEER-B", &identity.key_id());
        let mut ta = a.into_transport();
        let mut tb = b.into_transport();

        let before = sess.ws.outbound.len();
        assert_eq!(sess.broadcast(b"evt"), 2, "reached both peers");
        // Exactly one env per peer, each addressed to that peer.
        let new: Vec<RelayFrame> = sess.ws.outbound[before..]
            .iter()
            .map(|s| parse_frame(s).unwrap())
            .collect();
        assert_eq!(new.len(), 2, "one env per established peer");
        let ct_for = |peer: &str| -> Vec<u8> {
            for f in &new {
                if let RelayFrame::Env { dst, payload, src } = f {
                    assert!(src.is_none());
                    if dst == peer {
                        return unb64url(payload);
                    }
                }
            }
            panic!("no broadcast env for {peer}");
        };
        let ct_a = ct_for("PEER-A");
        let ct_b = ct_for("PEER-B");

        // Each initiator decrypts ITS OWN copy to b"evt".
        assert_eq!(ts_decrypt(&mut ta, &ct_a).unwrap(), b"evt");
        assert_eq!(ts_decrypt(&mut tb, &ct_b).unwrap(), b"evt");

        // Cross-decrypt is impossible: B's transport CANNOT read the ciphertext
        // the agent produced for A — the two transports have independent
        // keys/nonces, so A's broadcast copy is opaque to B.
        assert!(
            ts_decrypt(&mut tb, &ct_a).is_err(),
            "B's transport must NOT decrypt A's broadcast ciphertext"
        );
    }

    #[test]
    fn garbage_transport_frame_drops_only_that_peer() {
        let identity = AgentIdentity::generate();
        let agent_pub = identity.agreement_public_raw();
        let allowed: HashSet<String> = ["PEER-A", "PEER-B"].iter().map(|s| s.to_string()).collect();
        let mut sess = admit(&identity, allowed);

        let mut a = TestInitiator::new(&[0xAA; 32], &agent_pub);
        let mut b = TestInitiator::new(&[0xBB; 32], &agent_pub);
        bootstrap(&mut sess, &mut a, "PEER-A", &identity.key_id());
        bootstrap(&mut sess, &mut b, "PEER-B", &identity.key_id());
        let _ta = a.into_transport();
        let mut tb = b.into_transport();

        // A valid-base64 junk payload from A fails A's transport decrypt →
        // PeerDown(A), a per-peer failure (NOT a session error).
        sess.ws.push_inbound(env_json(
            &identity.key_id(),
            b"this-is-not-a-valid-noise-frame",
            Some("PEER-A"),
        ));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::PeerDown {
                peer: "PEER-A".to_string()
            }
        );
        assert!(sess.session_id("PEER-A").is_none(), "A removed");
        assert!(sess.session_id("PEER-B").is_some(), "B untouched");

        // B still decrypts fine — its state was never perturbed.
        let ct_b = ts_encrypt(&mut tb, b"unaffected");
        sess.ws
            .push_inbound(env_json(&identity.key_id(), &ct_b, Some("PEER-B")));
        assert_eq!(
            sess.process_next(TIMEOUT).unwrap(),
            Poll::Plaintext {
                peer: "PEER-B".to_string(),
                plaintext: b"unaffected".to_vec()
            }
        );
    }
}
