//! Agent cryptographic identity: Ed25519 identity key + X25519 static agreement
//! key, plus keyId derivation and domain-separated signing.
//!
//! Security invariants:
//! - Private keys are persisted with 0600 permissions and NEVER logged.
//! - The secret-holding struct does not derive `Debug`; the manual impl redacts
//!   all private material (prints only the public `key_id()` and public halves).
//! - keyId matches the pairing.v1 contract: unpadded base64url SHA-256 of the
//!   Ed25519 identity SubjectPublicKeyInfo DER
//!   (`302a300506032b6570032100` ‖ 32-byte raw key).

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use rand_core::OsRng;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret as XStaticSecret};

/// The 12-byte DER prefix of an Ed25519 SubjectPublicKeyInfo (SPKI). The raw
/// 32-byte public key is appended to this to form the full SPKI DER that keyId
/// is computed over. Equal to `hex::decode("302a300506032b6570032100")`.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

const IDENTITY_KEY_FILE: &str = "identity.key";
const AGREEMENT_KEY_FILE: &str = "agreement.key";

/// Errors from loading/persisting/using an [`AgentIdentity`].
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// Filesystem I/O failure. The path is included but never the key bytes.
    #[error("key I/O error at {path}: {source}")]
    Io {
        /// The path involved.
        path: String,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// A persisted key file did not contain exactly 32 bytes.
    #[error("malformed key file {path}: expected 32 bytes, got {len}")]
    MalformedKeyFile {
        /// The path involved.
        path: String,
        /// The actual length read.
        len: usize,
    },
}

/// Encode bytes as unpadded base64url.
fn b64url_unpadded(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// An agent's cryptographic identity: an Ed25519 signing key (identity) and an
/// X25519 static secret (Noise IK agreement key).
///
/// Does NOT derive `Debug` — the manual impl redacts all secret material.
pub struct AgentIdentity {
    identity: SigningKey,
    agreement: XStaticSecret,
}

impl AgentIdentity {
    /// Generate a fresh identity from the OS CSPRNG.
    pub fn generate() -> Self {
        let identity = SigningKey::generate(&mut OsRng);
        let agreement = XStaticSecret::random_from_rng(OsRng);
        Self {
            identity,
            agreement,
        }
    }

    /// Load an existing identity from `dir`, or generate + persist a new one if
    /// the key files do not exist.
    ///
    /// Key files are written raw (32 bytes each) with 0600 permissions; `dir`
    /// is created with 0700 permissions if missing.
    pub fn load_or_generate(dir: &Path) -> Result<Self, KeyError> {
        let identity_path = dir.join(IDENTITY_KEY_FILE);
        let agreement_path = dir.join(AGREEMENT_KEY_FILE);

        if identity_path.exists() && agreement_path.exists() {
            let id_bytes = read_key_file(&identity_path)?;
            let agr_bytes = read_key_file(&agreement_path)?;
            let identity = SigningKey::from_bytes(&id_bytes);
            let agreement = XStaticSecret::from(agr_bytes);
            return Ok(Self {
                identity,
                agreement,
            });
        }

        // Generate fresh and persist.
        let me = Self::generate();
        me.persist(dir)?;
        Ok(me)
    }

    /// Persist both raw private keys to `dir` with 0600 perms, creating `dir`
    /// (0700) if needed.
    fn persist(&self, dir: &Path) -> Result<(), KeyError> {
        if !dir.exists() {
            fs::create_dir_all(dir).map_err(|e| KeyError::Io {
                path: dir.display().to_string(),
                source: e,
            })?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| {
                KeyError::Io {
                    path: dir.display().to_string(),
                    source: e,
                }
            })?;
        }

        let identity_path = dir.join(IDENTITY_KEY_FILE);
        let agreement_path = dir.join(AGREEMENT_KEY_FILE);

        write_key_file(&identity_path, &self.identity.to_bytes())?;
        write_key_file(&agreement_path, &self.agreement.to_bytes())?;
        Ok(())
    }

    /// The raw 32-byte Ed25519 identity public key.
    pub fn identity_public_raw(&self) -> [u8; 32] {
        self.identity.verifying_key().to_bytes()
    }

    /// The raw 32-byte X25519 static agreement public key.
    pub fn agreement_public_raw(&self) -> [u8; 32] {
        XPublicKey::from(&self.agreement).to_bytes()
    }

    /// The raw 32-byte X25519 static agreement **private** key, needed to build
    /// the Noise IK responder handshake (the agent's production role).
    ///
    /// Secret material: callers must not log or persist the return value beyond
    /// what `snow` requires. It is exposed only so the session driver can seed
    /// [`crate::noise::ResponderHandshake::new`]; it is never rendered by the
    /// redacting `Debug` impl.
    pub fn agreement_private_bytes(&self) -> [u8; 32] {
        self.agreement.to_bytes()
    }

    /// The agent's keyId: unpadded base64url SHA-256 of the Ed25519 identity
    /// SPKI DER (`302a300506032b6570032100` ‖ 32-byte raw key).
    pub fn key_id(&self) -> String {
        let mut spki = Vec::with_capacity(ED25519_SPKI_PREFIX.len() + 32);
        spki.extend_from_slice(&ED25519_SPKI_PREFIX);
        spki.extend_from_slice(&self.identity_public_raw());
        let digest = Sha256::digest(&spki);
        b64url_unpadded(&digest)
    }

    /// Sign `msg` with the identity key under a domain-separation `tag`:
    /// Ed25519 over `tag.as_bytes() ‖ 0x00 ‖ msg`.
    pub fn sign_domain(&self, tag: &str, msg: &[u8]) -> [u8; 64] {
        let mut buf = Vec::with_capacity(tag.len() + 1 + msg.len());
        buf.extend_from_slice(tag.as_bytes());
        buf.push(0x00);
        buf.extend_from_slice(msg);
        self.identity.sign(&buf).to_bytes()
    }

    /// The Ed25519 verifying (public) key, for signature verification.
    pub fn verifying_key(&self) -> ed25519_dalek::VerifyingKey {
        self.identity.verifying_key()
    }
}

impl fmt::Debug for AgentIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redact all secret material: print only public identifiers.
        f.debug_struct("AgentIdentity")
            .field("key_id", &self.key_id())
            .field(
                "identity_public",
                &b64url_unpadded(&self.identity_public_raw()),
            )
            .field(
                "agreement_public",
                &b64url_unpadded(&self.agreement_public_raw()),
            )
            .finish_non_exhaustive()
    }
}

/// Read a raw 32-byte key file.
fn read_key_file(path: &Path) -> Result<[u8; 32], KeyError> {
    let bytes = fs::read(path).map_err(|e| KeyError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    let len = bytes.len();
    let arr: [u8; 32] = bytes.try_into().map_err(|_| KeyError::MalformedKeyFile {
        path: path.display().to_string(),
        len,
    })?;
    Ok(arr)
}

/// Write a raw key file with 0600 permissions.
fn write_key_file(path: &Path, bytes: &[u8; 32]) -> Result<(), KeyError> {
    fs::write(path, bytes).map_err(|e| KeyError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| KeyError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "pancetta-agent-keys-test-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn spki_prefix_matches_hex() {
        let expected = hex::decode("302a300506032b6570032100").unwrap();
        assert_eq!(&ED25519_SPKI_PREFIX[..], &expected[..]);
    }

    #[test]
    fn key_id_stable_across_persist_and_reload() {
        let dir = temp_dir("reload");
        let a = AgentIdentity::load_or_generate(&dir).unwrap();
        let key_id_a = a.key_id();
        let id_pub_a = a.identity_public_raw();
        let agr_pub_a = a.agreement_public_raw();
        drop(a);

        // Reload from the same dir — files exist, so it must load, not regen.
        let b = AgentIdentity::load_or_generate(&dir).unwrap();
        assert_eq!(key_id_a, b.key_id(), "keyId must survive persist+reload");
        assert_eq!(id_pub_a, b.identity_public_raw());
        assert_eq!(agr_pub_a, b.agreement_public_raw());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persisted_key_files_are_mode_0600() {
        let dir = temp_dir("perms");
        let _ = AgentIdentity::load_or_generate(&dir).unwrap();

        for f in [IDENTITY_KEY_FILE, AGREEMENT_KEY_FILE] {
            let path = dir.join(f);
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{f} must be 0600, got {mode:o}");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sign_domain_is_domain_separated() {
        let id = AgentIdentity::generate();
        let vk = id.verifying_key();
        let c = b"the-same-agreement-key-bytes-here!!";

        let tag_relay = "cqdx-relay-agent-auth-v1";
        let tag_idsig = "cqdx-pair-idsig-v1";

        let sig_relay = id.sign_domain(tag_relay, c);
        let sig_idsig = id.sign_domain(tag_idsig, c);

        // Different tags over the SAME message → different signatures.
        assert_ne!(
            sig_relay, sig_idsig,
            "domain separation must yield distinct signatures"
        );

        // Each signature verifies only under its own domain-separated message.
        let msg_relay = {
            let mut m = tag_relay.as_bytes().to_vec();
            m.push(0x00);
            m.extend_from_slice(c);
            m
        };
        let msg_idsig = {
            let mut m = tag_idsig.as_bytes().to_vec();
            m.push(0x00);
            m.extend_from_slice(c);
            m
        };

        let s_relay = Signature::from_bytes(&sig_relay);
        let s_idsig = Signature::from_bytes(&sig_idsig);

        assert!(vk.verify(&msg_relay, &s_relay).is_ok());
        assert!(vk.verify(&msg_idsig, &s_idsig).is_ok());
        // Cross-verification must fail (proves the tag is bound into the sig).
        assert!(vk.verify(&msg_idsig, &s_relay).is_err());
        assert!(vk.verify(&msg_relay, &s_idsig).is_err());
    }

    #[test]
    fn key_id_matches_manual_spki_sha256() {
        let id = AgentIdentity::generate();
        let mut spki = hex::decode("302a300506032b6570032100").unwrap();
        spki.extend_from_slice(&id.identity_public_raw());
        let digest = Sha256::digest(&spki);
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        assert_eq!(id.key_id(), expected);
    }

    #[test]
    fn debug_impl_redacts_secrets() {
        let id = AgentIdentity::generate();
        let dbg = format!("{id:?}");
        // Public key_id present; no raw secret bytes leaked. We can't assert
        // absence of arbitrary bytes, but we assert the secret fields aren't
        // named in the Debug output.
        assert!(dbg.contains("key_id"));
        assert!(!dbg.contains("identity: "));
        assert!(!dbg.contains("agreement: SigningKey"));
    }
}
