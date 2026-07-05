//! Agent-leg session driver: relay auth handshake + Noise-over-`env`.
//!
//! [`AgentSession`] drives the agent's side of a relay connection over a
//! [`WsConn`] seam, transport-agnostic so it can be unit-tested with a scripted
//! mock. The leg is:
//!
//! 1. Recv `hello{challenge}` → send `auth{role:"agent", agentKeyId, sig}` where
//!    `sig = domainSeparate("cqdx-relay-agent-auth-v1", challengeBytes)`.
//! 2. Recv `ready{keyId, peerPresent}` → admitted; observe `presence`.
//! 3. Noise-over-`env`: the first inbound `env.payload` (once the peer is
//!    present) is Noise **msg1** → responder `read_msg1` → `write_msg2(&[])` →
//!    send back as `env{dst: clientKeyId, payload}` → `into_transport`.
//!    Thereafter each inbound `env.payload` decrypts to a control frame; each
//!    outbound control frame encrypts into an `env`.
//! 4. `src` guard: an inbound `env` whose `src` is present and != the client
//!    peer keyId is dropped.
//!
//! Error handling: a terminal `error` stops the session and surfaces; a
//! transient `error` is ignored (keep waiting). The dial/reconnect loop is P3.4.

use crate::keys::AgentIdentity;
use crate::noise::{NoiseTransport, ResponderHandshake};
use crate::relay::{
    decode_env_payload, encode_env_payload, is_terminal, parse_frame, RelayError, RelayFrame,
    WsConn,
};

/// Domain-separation tag for the relay agent-auth signature (relay.v1 auth.sig,
/// Q-0011 pancetta refinement).
const AUTH_DOMAIN_TAG: &str = "cqdx-relay-agent-auth-v1";

/// Encode bytes as unpadded base64url (matches `keys::b64url_unpadded`'s
/// convention; not shared across modules since each is `pub(crate)`-private).
fn b64url_unpadded(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Errors from driving the agent session leg.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// A relay-layer error (bad frame, oversized, bad payload, transport).
    #[error("relay error: {0}")]
    Relay(#[from] RelayError),

    /// A Noise handshake/transport error.
    #[error("noise error: {0}")]
    Noise(#[from] crate::noise::NoiseError),

    /// The peer closed the connection before the expected frame arrived.
    #[error("connection closed unexpectedly during {0}")]
    UnexpectedClose(&'static str),

    /// Received a frame that is invalid for the current session phase.
    #[error("unexpected frame during {phase}: {got}")]
    UnexpectedFrame {
        /// The session phase we were in.
        phase: &'static str,
        /// A short description of the frame received.
        got: String,
    },

    /// The DO sent a terminal `error` frame; the session must not retry.
    #[error("terminal error from relay: {code}")]
    Terminal {
        /// The terminal error code.
        code: String,
    },

    /// The challenge in `hello` did not decode as base64url.
    #[error("bad challenge encoding: {0}")]
    BadChallenge(String),
}

/// Where in the leg the session currently is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Sent `auth`, awaiting `ready`.
    AwaitingReady,
    /// Admitted; awaiting the client's Noise msg1 (first `env`).
    AwaitingMsg1,
    /// Noise transport established; exchanging encrypted control frames.
    Transport,
}

/// Drives the agent side of a relay connection over a [`WsConn`].
///
/// Generic over the WS seam so tests use a scripted mock and production (P3.4)
/// uses a real async socket adapter.
pub struct AgentSession<'a, W: WsConn> {
    ws: W,
    identity: &'a AgentIdentity,
    /// The client peer's keyId. Used as `env.dst` and to validate `env.src`.
    /// May be supplied at construction (an operator-pinned expected peer) OR
    /// **empty**, in which case the agent learns it from the DO-authenticated
    /// `src` of the first inbound `env` (the responder cannot know the client
    /// until it connects). Once set (either way) it is enforced on later frames.
    client_key_id: String,
    phase: Option<Phase>,
    /// The responder handshake, consumed on msg1 → transport transition.
    handshake: Option<ResponderHandshake>,
    /// The established transport, present once the handshake completes.
    transport: Option<NoiseTransport>,
    /// Unpadded base64url of the Noise handshake hash (`h`), captured the
    /// instant the handshake completes (dispensa Q-0022) — the channel
    /// binding a `txArmGrant.sessionId` is checked against. `None` until the
    /// transport is established; naturally resets on reconnect since a fresh
    /// `AgentSession` is built per connection.
    session_id: Option<String>,
}

impl<'a, W: WsConn> AgentSession<'a, W> {
    /// Create a session driver over `ws` for `identity`, expecting the peer
    /// `client_key_id`.
    pub fn new(ws: W, identity: &'a AgentIdentity, client_key_id: String) -> Self {
        Self {
            ws,
            identity,
            client_key_id,
            phase: None,
            handshake: None,
            transport: None,
            session_id: None,
        }
    }

    /// Whether the Noise transport is established (admitted + handshaked).
    pub fn is_transport_established(&self) -> bool {
        matches!(self.phase, Some(Phase::Transport))
    }

    /// The channel-binding session id (dispensa Q-0022): unpadded base64url
    /// of the Noise handshake hash, present once the transport is
    /// established. This is what a `txArmGrant.sessionId` must equal — since
    /// it's derived from the completed handshake transcript, the relay
    /// cannot forge or substitute it, and a grant signed for one session can
    /// never verify against another.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Whether the session has been admitted (received `ready`).
    pub fn is_admitted(&self) -> bool {
        matches!(
            self.phase,
            Some(Phase::AwaitingMsg1) | Some(Phase::Transport)
        )
    }

    /// Recv `hello`, then send `auth`. Advances to [`Phase::AwaitingReady`].
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
        self.phase = Some(Phase::AwaitingReady);
        Ok(())
    }

    /// Process a single inbound frame, driving the leg forward. Returns any
    /// decrypted control-frame plaintext produced by this step (from a transport
    /// `env`), or `None` for handshake/status frames. Returns
    /// `Ok(None)` on a benign frame; the caller loops.
    ///
    /// A terminal `error` returns [`SessionError::Terminal`]. A transient error
    /// is ignored (returns `Ok(None)`).
    pub fn process_next(&mut self) -> Result<Option<Vec<u8>>, SessionError> {
        let text = match self.ws.recv_text()? {
            Some(t) => t,
            None => return Ok(None), // connection drained; caller decides.
        };
        let frame = parse_frame(&text)?;
        self.process_frame(frame)
    }

    /// Handle one parsed frame per the current phase.
    fn process_frame(&mut self, frame: RelayFrame) -> Result<Option<Vec<u8>>, SessionError> {
        match frame {
            RelayFrame::Ready { peer_present, .. } => {
                // Admitted. Move to awaiting msg1 regardless of peerPresent —
                // the client's first env arrives once it's up.
                let _ = peer_present;
                self.phase = Some(Phase::AwaitingMsg1);
                Ok(None)
            }
            RelayFrame::Presence { .. } => {
                // Informational: we act on the first env, not on presence.
                Ok(None)
            }
            RelayFrame::Env { payload, src, .. } => self.process_env(&payload, src.as_deref()),
            RelayFrame::Error { code, .. } => self.handle_error_frame(code).map(|()| None),
            RelayFrame::Bye { .. } => Err(SessionError::UnexpectedClose("peer sent bye")),
            other @ (RelayFrame::Hello { .. } | RelayFrame::Auth { .. }) => {
                Err(SessionError::UnexpectedFrame {
                    phase: "post-auth",
                    got: describe(&other),
                })
            }
        }
    }

    /// Process an inbound `env`: msg1 → transport bootstrap, or a transport
    /// ciphertext → decrypted plaintext. Learns the peer keyId from the first
    /// `env.src` (the agent is the responder and cannot know the client's keyId
    /// at construction), then applies the `src` guard on every later frame.
    fn process_env(
        &mut self,
        payload_b64: &str,
        src: Option<&str>,
    ) -> Result<Option<Vec<u8>>, SessionError> {
        // Peer learning: the agent (Noise responder) does not know the client's
        // keyId until it connects. The DO authenticates the client on admission
        // and stamps the authenticated `src` on every forwarded `env`, so the
        // first `src` is a trustworthy peer identity. Latch it if we don't yet
        // have one; thereafter it also becomes our `env.dst` (msg2/replies).
        // NOTE (security): learning the peer does NOT authorize TX — arming is
        // gated separately by the station-local TX-allow-list in `capability`.
        // src guard. The DO stamps an authenticated `src` on EVERY forwarded
        // env, so an inbound env with no `src` is anomalous and unattributable —
        // drop it rather than trust an unidentified peer. Otherwise: learn the
        // peer from the first src (responder can't know it at construction), and
        // thereafter require every env's src to match.
        match src {
            None => return Ok(None),
            Some(s) if self.client_key_id.is_empty() => {
                // Peer learning — the DO already authenticated this src. NOTE
                // (security): learning the peer does NOT authorize TX; arming is
                // gated separately by the station-local TX-allow-list.
                self.client_key_id = s.to_string();
            }
            Some(s) if s != self.client_key_id => {
                // Cross-peer / spoofed routing for a known peer — drop.
                return Ok(None);
            }
            Some(_) => {}
        }
        let payload = decode_env_payload(payload_b64)?;

        match self.phase {
            Some(Phase::AwaitingMsg1) | Some(Phase::AwaitingReady) => {
                // Treat the first env as Noise msg1. (If msg1 arrives before
                // ready — it cannot on the real relay — we still bootstrap.)
                let mut hs = self
                    .handshake
                    .take()
                    .map(Ok)
                    .unwrap_or_else(|| ResponderHandshake::new(&self.agreement_priv()))?;
                let _payload = hs.read_msg1(&payload)?;
                let msg2 = hs.write_msg2(&[])?;
                let out = RelayFrame::Env {
                    dst: self.client_key_id.clone(),
                    payload: encode_env_payload(&msg2),
                    src: None,
                };
                self.ws.send_text(out.to_json()?)?;
                // Capture the handshake hash BEFORE `into_transport` consumes
                // `hs` — it's only available pre-transport (Q-0022 channel
                // binding).
                self.session_id = Some(b64url_unpadded(&hs.handshake_hash()));
                self.transport = Some(hs.into_transport()?);
                self.phase = Some(Phase::Transport);
                Ok(None)
            }
            Some(Phase::Transport) => {
                let transport = self
                    .transport
                    .as_mut()
                    .expect("transport present in Transport phase");
                let plaintext = transport.decrypt(&payload)?;
                Ok(Some(plaintext))
            }
            None => Err(SessionError::UnexpectedFrame {
                phase: "pre-auth",
                got: "env".to_string(),
            }),
        }
    }

    /// Encrypt `plaintext` as a control frame and send it as an `env` to the
    /// client peer. Only valid once the transport is established.
    pub fn send_control(&mut self, plaintext: &[u8]) -> Result<(), SessionError> {
        let transport = self
            .transport
            .as_mut()
            .ok_or(SessionError::UnexpectedFrame {
                phase: "send_control",
                got: "transport not established".to_string(),
            })?;
        let ct = transport.encrypt(plaintext)?;
        let env = RelayFrame::Env {
            dst: self.client_key_id.clone(),
            payload: encode_env_payload(&ct),
            src: None,
        };
        self.ws.send_text(env.to_json()?)?;
        Ok(())
    }

    /// Map an `error` frame to keep-waiting (transient) or stop (terminal).
    fn handle_error_frame(&self, code: String) -> Result<(), SessionError> {
        if is_terminal(&code) {
            Err(SessionError::Terminal { code })
        } else {
            // Transient: caller keeps waiting.
            Ok(())
        }
    }

    /// The agent's X25519 static private key bytes (Noise responder key).
    fn agreement_priv(&self) -> [u8; 32] {
        // AgentIdentity does not expose the private agreement bytes directly to
        // keep secrets contained; the ResponderHandshake needs them. We rebuild
        // from the identity via the dedicated accessor.
        self.identity.agreement_private_bytes()
    }
}

/// A short human description of a frame for error messages (no secret content).
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
    use crate::noise::ResponderHandshake;
    use base64::Engine as _;
    use ed25519_dalek::{Signature, Verifier};

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
        fn send_text(&mut self, s: String) -> Result<(), RelayError> {
            self.outbound.push(s);
            Ok(())
        }
        fn recv_text(&mut self) -> Result<Option<String>, RelayError> {
            Ok(self.inbound.pop_front())
        }
    }

    /// A test-only Noise IK initiator (the client side), mirroring the cfg(test)
    /// helper in noise.rs.
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

    fn client_key_id() -> String {
        "CLIENT-KEY-ID".to_string()
    }

    /// Cross-repo conformance check for dispensa Q-0022: pancetta's production
    /// `b64url_unpadded` (the same function `session_id()` uses on the real
    /// handshake hash) must reproduce `sessionIdB64` from
    /// `contracts/auth/tx-arm-session-id.vectors.v1.json` byte-for-byte given
    /// that vector's `handshakeHashHex`. This is the "not yet checked against
    /// this specific vector file" gap the vector's own `conformance.pancetta`
    /// note called out; panino already PASSed the same fixture.
    #[test]
    fn session_id_matches_dispensa_q0022_vector() {
        let handshake_hash_hex = "48f3cb8bc9319da4ba1e9933991b1c4ed4034f1f126a76d3a1fbcfd7f94248d4";
        let expected_session_id_b64 = "SPPLi8kxnaS6HpkzmRscTtQDTx8SanbTofvP1_lCSNQ";

        let hash_bytes = hex_decode(handshake_hash_hex);
        assert_eq!(hash_bytes.len(), 32, "handshake hash must be 32 bytes");
        assert_eq!(b64url_unpadded(&hash_bytes), expected_session_id_b64);
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn full_agent_leg_reaches_admitted_and_auth_sig_verifies() {
        let identity = AgentIdentity::generate();
        let challenge = vec![7u8; 32];
        let hello = RelayFrame::Hello {
            challenge: b64url(&challenge),
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
        let mut sess = AgentSession::new(ws, &identity, client_key_id());

        sess.authenticate().unwrap();
        // process the ready frame → admitted
        assert_eq!(sess.process_next().unwrap(), None);
        assert!(sess.is_admitted());

        // Verify the auth frame captured on the wire.
        let auth_json = &sess.ws.outbound[0];
        let parsed = parse_frame(auth_json).unwrap();
        let (role, agent_key_id, sig) = match parsed {
            RelayFrame::Auth {
                role,
                agent_key_id,
                sig,
            } => (role, agent_key_id, sig),
            _ => panic!("expected auth"),
        };
        assert_eq!(role, "agent");
        assert_eq!(agent_key_id, identity.key_id());

        // The sig verifies over domainSeparate(tag, challenge).
        let mut signed_msg = AUTH_DOMAIN_TAG.as_bytes().to_vec();
        signed_msg.push(0x00);
        signed_msg.extend_from_slice(&challenge);
        let sig_bytes = unb64url(&sig);
        let signature = Signature::from_slice(&sig_bytes).unwrap();
        identity
            .verifying_key()
            .verify(&signed_msg, &signature)
            .expect("auth sig must verify over the domain-separated challenge");
    }

    #[test]
    fn noise_over_env_round_trips_end_to_end() {
        let identity = AgentIdentity::generate();
        let agent_static_pub = identity.agreement_public_raw();

        // Build a client-side initiator that knows the agent's static pub key.
        let client_kp = {
            let params: snow::params::NoiseParams =
                "Noise_IK_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
            snow::Builder::new(params).generate_keypair().unwrap()
        };
        let mut initiator = TestInitiator::new(&client_kp.private, &agent_static_pub);
        let msg1 = initiator.write_msg1(b"");

        let challenge = vec![3u8; 32];
        let hello = RelayFrame::Hello {
            challenge: b64url(&challenge),
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
            src: Some(client_key_id()),
        }
        .to_json()
        .unwrap();

        let ws = MockWs::new(vec![hello, ready, env_msg1]);
        let mut sess = AgentSession::new(ws, &identity, client_key_id());

        sess.authenticate().unwrap();
        sess.process_next().unwrap(); // ready
        sess.process_next().unwrap(); // env(msg1) → sends msg2, transport up
        assert!(sess.is_transport_established());

        // The agent's outbound[1] is the env carrying msg2. Feed it to the
        // initiator to complete the handshake.
        let out_env = parse_frame(&sess.ws.outbound[1]).unwrap();
        let msg2 = match out_env {
            RelayFrame::Env { dst, payload, src } => {
                assert_eq!(dst, client_key_id(), "agent addresses the client peer");
                assert!(src.is_none(), "senders never set src");
                unb64url(&payload)
            }
            _ => panic!("expected env msg2"),
        };
        initiator.read_msg2(&msg2);

        // Q-0022: both sides derive the SAME session_id (unpadded base64url
        // of the Noise handshake hash) from the completed handshake alone —
        // no wire field, no coordination beyond the handshake itself.
        let client_hash = initiator.handshake_hash();
        assert_eq!(
            sess.session_id(),
            Some(b64url(&client_hash)).as_deref(),
            "agent and client must derive an identical channel-binding session_id"
        );

        let mut client_transport = initiator.into_transport();

        // Client → agent: encrypt a control frame, deliver as env, agent decrypts.
        let control = b"{\"cmd\":\"status\"}";
        let mut ct = vec![0u8; control.len() + 16 + 1];
        let n = client_transport.write_message(control, &mut ct).unwrap();
        ct.truncate(n);
        let inbound_env = RelayFrame::Env {
            dst: identity.key_id(),
            payload: b64url(&ct),
            src: Some(client_key_id()),
        }
        .to_json()
        .unwrap();
        sess.ws.push_inbound(inbound_env);
        let got = sess.process_next().unwrap();
        assert_eq!(got.as_deref(), Some(&control[..]));

        // Agent → client: send_control, then decrypt on the client side.
        let reply = b"{\"ok\":true}";
        sess.send_control(reply).unwrap();
        let reply_env = parse_frame(sess.ws.outbound.last().unwrap()).unwrap();
        let reply_ct = match reply_env {
            RelayFrame::Env { payload, .. } => unb64url(&payload),
            _ => panic!("expected env"),
        };
        let mut buf = vec![0u8; reply_ct.len().max(1)];
        let n = client_transport.read_message(&reply_ct, &mut buf).unwrap();
        buf.truncate(n);
        assert_eq!(&buf[..], &reply[..]);
    }

    #[test]
    fn env_with_wrong_src_is_dropped() {
        let identity = AgentIdentity::generate();
        // Get the session admitted first.
        let hello = RelayFrame::Hello {
            challenge: b64url(&[1u8; 32]),
        }
        .to_json()
        .unwrap();
        let ready = RelayFrame::Ready {
            key_id: identity.key_id(),
            peer_present: true,
        }
        .to_json()
        .unwrap();
        // An env from an impostor src — must be dropped, not bootstrap Noise.
        let bad_env = RelayFrame::Env {
            dst: identity.key_id(),
            payload: b64url(b"garbage-msg1"),
            src: Some("IMPOSTOR".to_string()),
        }
        .to_json()
        .unwrap();

        let ws = MockWs::new(vec![hello, ready, bad_env]);
        let mut sess = AgentSession::new(ws, &identity, client_key_id());
        sess.authenticate().unwrap();
        sess.process_next().unwrap(); // ready
        let out = sess.process_next().unwrap(); // bad env dropped
        assert_eq!(out, None);
        assert!(
            !sess.is_transport_established(),
            "impostor env must not bootstrap the Noise handshake"
        );
    }

    #[test]
    fn env_with_absent_src_is_dropped() {
        // An env with NO `src` stamp is unattributable (the DO stamps src on all
        // forwards) — it must be dropped, never bootstrap Noise, even though the
        // session has a pinned peer.
        let identity = AgentIdentity::generate();
        let hello = RelayFrame::Hello {
            challenge: b64url(&[1u8; 32]),
        }
        .to_json()
        .unwrap();
        let ready = RelayFrame::Ready {
            key_id: identity.key_id(),
            peer_present: true,
        }
        .to_json()
        .unwrap();
        let no_src_env = RelayFrame::Env {
            dst: identity.key_id(),
            payload: b64url(b"garbage-msg1"),
            src: None,
        }
        .to_json()
        .unwrap();

        let ws = MockWs::new(vec![hello, ready, no_src_env]);
        let mut sess = AgentSession::new(ws, &identity, client_key_id());
        sess.authenticate().unwrap();
        sess.process_next().unwrap(); // ready
        let out = sess.process_next().unwrap(); // src-less env dropped
        assert_eq!(out, None);
        assert!(
            !sess.is_transport_established(),
            "src-less env must not bootstrap the Noise handshake"
        );
    }

    #[test]
    fn agent_learns_peer_keyid_from_first_env_src() {
        // Constructed WITHOUT a pinned peer (empty client_key_id): the agent
        // must LEARN the peer from the DO-authenticated `src` of the first env
        // (Noise msg1), not drop it. Then it enforces that peer on later frames.
        let identity = AgentIdentity::generate();
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
        let peer = "LEARNED-PEER-KEYID".to_string();
        let env_msg1 = RelayFrame::Env {
            dst: identity.key_id(),
            payload: b64url(&msg1),
            src: Some(peer.clone()),
        }
        .to_json()
        .unwrap();

        let ws = MockWs::new(vec![hello, ready, env_msg1]);
        // EMPTY client_key_id — the agent must learn it from the first env.
        let mut sess = AgentSession::new(ws, &identity, String::new());
        sess.authenticate().unwrap();
        sess.process_next().unwrap(); // ready
        sess.process_next().unwrap(); // env(msg1): learn peer + bootstrap Noise
        assert!(
            sess.is_transport_established(),
            "first env with a DO-stamped src must be learned, not dropped"
        );
        // msg2 is addressed to the learned peer.
        match parse_frame(&sess.ws.outbound[1]).unwrap() {
            RelayFrame::Env { dst, .. } => {
                assert_eq!(dst, peer, "agent addresses the learned peer as env.dst");
            }
            _ => panic!("expected env msg2"),
        }
        // After learning, an env from a DIFFERENT src is dropped (peer enforced).
        let bad = RelayFrame::Env {
            dst: identity.key_id(),
            payload: b64url(b"x"),
            src: Some("OTHER-SRC".to_string()),
        }
        .to_json()
        .unwrap();
        sess.ws.push_inbound(bad);
        assert_eq!(
            sess.process_next().unwrap(),
            None,
            "once the peer is learned, a different src is dropped"
        );
    }

    #[test]
    fn terminal_error_stops_session() {
        let identity = AgentIdentity::generate();
        let hello = RelayFrame::Hello {
            challenge: b64url(&[1u8; 32]),
        }
        .to_json()
        .unwrap();
        let err = RelayFrame::Error {
            code: "BAD_AUTH".to_string(),
            message: Some("nope".to_string()),
        }
        .to_json()
        .unwrap();
        let ws = MockWs::new(vec![hello, err]);
        let mut sess = AgentSession::new(ws, &identity, client_key_id());
        sess.authenticate().unwrap();
        let res = sess.process_next();
        assert!(matches!(res, Err(SessionError::Terminal { code }) if code == "BAD_AUTH"));
    }

    #[test]
    fn transient_error_is_ignored() {
        let identity = AgentIdentity::generate();
        let hello = RelayFrame::Hello {
            challenge: b64url(&[1u8; 32]),
        }
        .to_json()
        .unwrap();
        let err = RelayFrame::Error {
            code: "NO_PEER".to_string(),
            message: None,
        }
        .to_json()
        .unwrap();
        let ws = MockWs::new(vec![hello, err]);
        let mut sess = AgentSession::new(ws, &identity, client_key_id());
        sess.authenticate().unwrap();
        // Transient → keep waiting (Ok(None)), no panic.
        assert_eq!(sess.process_next().unwrap(), None);
    }

    #[test]
    fn malformed_frame_errors_without_panic() {
        let identity = AgentIdentity::generate();
        let ws = MockWs::new(vec!["{not valid json".to_string()]);
        let mut sess = AgentSession::new(ws, &identity, client_key_id());
        let res = sess.authenticate();
        assert!(matches!(
            res,
            Err(SessionError::Relay(RelayError::BadFrame(_)))
        ));
    }

    #[test]
    fn responder_handshake_uses_agent_static_key() {
        // Sanity: a ResponderHandshake built from the identity's agreement key
        // interoperates with an initiator keyed to the identity's public key.
        let identity = AgentIdentity::generate();
        let priv_bytes = identity.agreement_private_bytes();
        let mut responder = ResponderHandshake::new(&priv_bytes).unwrap();

        let client_kp = {
            let params: snow::params::NoiseParams =
                "Noise_IK_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
            snow::Builder::new(params).generate_keypair().unwrap()
        };
        let mut initiator =
            TestInitiator::new(&client_kp.private, &identity.agreement_public_raw());
        let msg1 = initiator.write_msg1(b"hi");
        assert_eq!(responder.read_msg1(&msg1).unwrap(), b"hi");
        let msg2 = responder.write_msg2(b"").unwrap();
        initiator.read_msg2(&msg2);
    }
}
