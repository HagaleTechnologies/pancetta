//! Noise IK handshake + transport for the pancetta station agent.
//!
//! A thin, misuse-resistant session wrapper over [`snow`] for the agent's
//! end-to-end encrypted channel. Suite:
//! `Noise_IK_25519_ChaChaPoly_BLAKE2s` (dispensa ADR-0002 / e2e-auth.v1).
//!
//! Role model: in production the agent is ALWAYS the **responder** — it holds
//! the static X25519 key the client learned during pairing. The public surface
//! is therefore responder-only ([`ResponderHandshake`] → [`NoiseTransport`]).
//! The initiator ([`InitiatorHandshake`]) exists only for loopback tests and
//! is gated behind `#[cfg(test)]`.
//!
//! Misuse-resistance:
//! - Each phase is a distinct type. [`ResponderHandshake::into_transport`]
//!   consumes `self` by value, so a completed handshake cannot be reused and
//!   the responder cannot process a second `msg1` after it has moved to
//!   transport mode (compile-time guarantee).
//! - The Noise per-message nonce is enforced by `snow`, so a replayed transport
//!   ciphertext fails to decrypt.
//!
//! Key material: static keys are held inside `snow`'s `HandshakeState` /
//! `Builder`. We do not add our own `zeroize` dependency for this offline phase.
//
// TODO(security): confirm snow zeroizes static key material on drop (snow 0.9
// zeroizes internal cipher/dh state via `Drop`; the `Builder`-held private key
// slice is caller-owned — callers should zeroize their own key buffers).

/// Noise suite used for the agent E2E channel.
const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

/// Upper bound on a single Noise message (handshake or transport) per RFC.
const MAX_MSG_LEN: usize = 65535;

/// Errors from the Noise session wrapper.
#[derive(thiserror::Error, Debug)]
pub enum NoiseError {
    /// Failed to build the handshake state (bad params or key length).
    #[error("noise handshake build failed: {0}")]
    Build(String),

    /// A handshake read/write step failed (e.g. bad msg1, auth failure).
    #[error("noise handshake step failed: {0}")]
    Handshake(String),

    /// Transitioning into transport mode failed (handshake not complete).
    #[error("noise transport transition failed: {0}")]
    Transport(String),

    /// A transport encrypt/decrypt operation failed (tamper, replay, or bad tag).
    #[error("noise transport cipher op failed: {0}")]
    Cipher(String),
}

/// Parse the fixed Noise suite params, mapping any error into [`NoiseError`].
fn params() -> Result<snow::params::NoiseParams, NoiseError> {
    NOISE_PARAMS
        .parse()
        .map_err(|e| NoiseError::Build(format!("params parse: {e}")))
}

/// Responder side of the IK handshake — the agent's production role.
///
/// Holds the agent's static X25519 private key. Drives `msg1` (read) then
/// `msg2` (write), after which [`into_transport`](Self::into_transport)
/// consumes the handshake and yields a [`NoiseTransport`].
pub struct ResponderHandshake {
    inner: snow::HandshakeState,
}

impl ResponderHandshake {
    /// Build a responder handshake from the agent's 32-byte X25519 private key.
    pub fn new(local_static_priv: &[u8]) -> Result<Self, NoiseError> {
        let inner = snow::Builder::new(params()?)
            .local_private_key(local_static_priv)
            .build_responder()
            .map_err(|e| NoiseError::Build(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Read handshake message 1 from the initiator, returning its decrypted
    /// payload (may be empty).
    pub fn read_msg1(&mut self, msg1: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; MAX_MSG_LEN];
        let n = self
            .inner
            .read_message(msg1, &mut buf)
            .map_err(|e| NoiseError::Handshake(format!("read msg1: {e}")))?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Write handshake message 2 (responder → initiator) carrying `payload`,
    /// returning the wire bytes.
    pub fn write_msg2(&mut self, payload: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; MAX_MSG_LEN];
        let n = self
            .inner
            .write_message(payload, &mut buf)
            .map_err(|e| NoiseError::Handshake(format!("write msg2: {e}")))?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Consume the completed handshake and enter transport mode.
    ///
    /// Taking `self` by value is the misuse-resistance lever: once transport
    /// mode is entered the handshake object no longer exists, so a second
    /// `msg1` cannot be processed against a live session.
    pub fn into_transport(self) -> Result<NoiseTransport, NoiseError> {
        let inner = self
            .inner
            .into_transport_mode()
            .map_err(|e| NoiseError::Transport(e.to_string()))?;
        Ok(NoiseTransport { inner })
    }

    /// The Noise handshake hash (`h`), available once the handshake is complete
    /// but before transport mode is entered. Useful as a channel-binding token.
    pub fn handshake_hash(&self) -> Vec<u8> {
        self.inner.get_handshake_hash().to_vec()
    }
}

/// Established transport session — AEAD encrypt/decrypt with Noise nonce
/// sequencing enforced by `snow`.
pub struct NoiseTransport {
    inner: snow::TransportState,
}

impl NoiseTransport {
    /// Encrypt `plaintext`, returning the ciphertext (plaintext + 16-byte tag).
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; plaintext.len() + 16 + 1];
        let n = self
            .inner
            .write_message(plaintext, &mut buf)
            .map_err(|e| NoiseError::Cipher(format!("encrypt: {e}")))?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Decrypt `ciphertext`, returning the recovered plaintext. Fails on tamper
    /// (bad tag), on replay (nonce already consumed), or on out-of-order
    /// delivery.
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; ciphertext.len().max(1)];
        let n = self
            .inner
            .read_message(ciphertext, &mut buf)
            .map_err(|e| NoiseError::Cipher(format!("decrypt: {e}")))?;
        buf.truncate(n);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Initiator side of the IK handshake — **test-only** (loopback / vectors).
    ///
    /// Production is responder-only; this helper lives behind `#[cfg(test)]`
    /// so it is never part of the shipped API surface.
    struct InitiatorHandshake {
        inner: snow::HandshakeState,
    }

    impl InitiatorHandshake {
        fn new(local_static_priv: &[u8], remote_static_pub: &[u8]) -> Result<Self, NoiseError> {
            let inner = snow::Builder::new(params()?)
                .local_private_key(local_static_priv)
                .remote_public_key(remote_static_pub)
                .build_initiator()
                .map_err(|e| NoiseError::Build(e.to_string()))?;
            Ok(Self { inner })
        }

        fn write_msg1(&mut self, payload: &[u8]) -> Result<Vec<u8>, NoiseError> {
            let mut buf = vec![0u8; MAX_MSG_LEN];
            let n = self
                .inner
                .write_message(payload, &mut buf)
                .map_err(|e| NoiseError::Handshake(format!("write msg1: {e}")))?;
            buf.truncate(n);
            Ok(buf)
        }

        fn read_msg2(&mut self, msg2: &[u8]) -> Result<Vec<u8>, NoiseError> {
            let mut buf = vec![0u8; MAX_MSG_LEN];
            let n = self
                .inner
                .read_message(msg2, &mut buf)
                .map_err(|e| NoiseError::Handshake(format!("read msg2: {e}")))?;
            buf.truncate(n);
            Ok(buf)
        }

        fn into_transport(self) -> Result<NoiseTransport, NoiseError> {
            let inner = self
                .inner
                .into_transport_mode()
                .map_err(|e| NoiseError::Transport(e.to_string()))?;
            Ok(NoiseTransport { inner })
        }
    }

    /// Generate a fresh X25519 static keypair for the suite.
    fn gen_keypair() -> snow::Keypair {
        snow::Builder::new(params().unwrap())
            .generate_keypair()
            .expect("generate keypair")
    }

    /// Drive a full IK handshake with random static keys and return both
    /// established transport sessions (initiator, responder).
    fn establish() -> (NoiseTransport, NoiseTransport) {
        let resp_kp = gen_keypair();
        let init_kp = gen_keypair();

        // Initiator must pre-know the responder's static PUBLIC key (IK).
        let mut initiator =
            InitiatorHandshake::new(&init_kp.private, &resp_kp.public).expect("build initiator");
        let mut responder = ResponderHandshake::new(&resp_kp.private).expect("build responder");

        let msg1 = initiator.write_msg1(b"hello").expect("write msg1");
        let got = responder.read_msg1(&msg1).expect("read msg1");
        assert_eq!(got, b"hello", "responder recovers msg1 payload");

        let msg2 = responder.write_msg2(b"welcome").expect("write msg2");
        let got = initiator.read_msg2(&msg2).expect("read msg2");
        assert_eq!(got, b"welcome", "initiator recovers msg2 payload");

        // Both sides derive the same handshake hash before split.
        assert_eq!(
            responder.handshake_hash(),
            initiator.inner.get_handshake_hash().to_vec(),
            "handshake hashes agree"
        );

        let init_t = initiator.into_transport().expect("init transport");
        let resp_t = responder.into_transport().expect("resp transport");
        (init_t, resp_t)
    }

    #[test]
    fn handshake_and_transport_loopback() {
        let (mut init_t, mut resp_t) = establish();

        // >= 3 messages EACH direction, round-tripping through encrypt/decrypt.
        for i in 0..4u8 {
            let m = format!("init->resp #{i}");
            let ct = init_t.encrypt(m.as_bytes()).expect("init encrypt");
            let pt = resp_t.decrypt(&ct).expect("resp decrypt");
            assert_eq!(pt, m.as_bytes(), "init->resp round-trip {i}");

            let m = format!("resp->init #{i}");
            let ct = resp_t.encrypt(m.as_bytes()).expect("resp encrypt");
            let pt = init_t.decrypt(&ct).expect("init decrypt");
            assert_eq!(pt, m.as_bytes(), "resp->init round-trip {i}");
        }
    }

    #[test]
    fn tampered_ciphertext_fails_decrypt() {
        let (mut init_t, mut resp_t) = establish();

        let mut ct = init_t.encrypt(b"authentic payload").expect("encrypt");
        // Flip a byte somewhere in the ciphertext body.
        ct[0] ^= 0xFF;

        let err = resp_t.decrypt(&ct);
        assert!(
            matches!(err, Err(NoiseError::Cipher(_))),
            "tampered ciphertext must fail AEAD auth, got {err:?}"
        );
    }

    #[test]
    fn replayed_transport_message_fails() {
        let (mut init_t, mut resp_t) = establish();

        let ct = init_t.encrypt(b"once and only once").expect("encrypt");

        // First delivery succeeds.
        let pt = resp_t.decrypt(&ct).expect("first delivery ok");
        assert_eq!(pt, b"once and only once");

        // Replaying the exact same bytes fails: the Noise receive nonce has
        // already advanced past this message.
        let replay = resp_t.decrypt(&ct);
        assert!(
            matches!(replay, Err(NoiseError::Cipher(_))),
            "replayed ciphertext must fail (nonce consumed), got {replay:?}"
        );
    }

    #[test]
    fn responder_rejects_second_msg1() {
        // Type-level guarantee: `into_transport(self)` consumes the handshake,
        // so a completed responder cannot be handed a second msg1 — there is no
        // handshake object left. The following would not compile:
        //
        //     let t = responder.into_transport()?;   // moves `responder`
        //     responder.read_msg1(&msg1_again);       // ERROR: use of moved value
        //
        // Runtime complement: a fresh responder that has already consumed msg1
        // and produced msg2 rejects a second msg1 (snow enforces handshake
        // ordering — the responder is out of read patterns).
        let resp_kp = gen_keypair();
        let init_kp = gen_keypair();

        let mut initiator =
            InitiatorHandshake::new(&init_kp.private, &resp_kp.public).expect("init");
        let mut responder = ResponderHandshake::new(&resp_kp.private).expect("resp");

        let msg1 = initiator.write_msg1(b"").expect("msg1");
        responder.read_msg1(&msg1).expect("first msg1 ok");
        let _msg2 = responder.write_msg2(b"").expect("msg2");

        // Handshake pattern is now exhausted for the responder; a second read
        // must error rather than silently accept a replayed/injected msg1.
        let second = responder.read_msg1(&msg1);
        assert!(
            matches!(second, Err(NoiseError::Handshake(_))),
            "responder must reject a second msg1 after handshake completes, got {second:?}"
        );
    }
}
