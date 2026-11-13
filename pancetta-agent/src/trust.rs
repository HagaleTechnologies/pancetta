//! Station-local device-key trust. Files here confer TX eligibility only
//! when intersected with the station's authorization edges.
use crate::keys::perms;
use base64::{engine::general_purpose, Engine as _};
use std::{fs, io, io::Write, path::Path};

pub fn validate_key_id(key_id: &str) -> io::Result<()> {
    if key_id.is_empty()
        || !key_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("keyId '{key_id}' has characters outside [A-Za-z0-9_-] (keyIds are unpadded base64url)")));
    }
    Ok(())
}

fn decode_key(bytes: &[u8]) -> io::Result<[u8; 32]> {
    if let Ok(raw) = bytes.try_into() {
        return Ok(raw);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        for engine in [
            &general_purpose::STANDARD,
            &general_purpose::STANDARD_NO_PAD,
            &general_purpose::URL_SAFE,
            &general_purpose::URL_SAFE_NO_PAD,
        ] {
            if let Ok(decoded) = engine.decode(text.trim()) {
                if let Ok(raw) = decoded.try_into() {
                    return Ok(raw);
                }
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "input does not hold a raw 32-byte or base64/base64url Ed25519 public key",
    ))
}

/// Publish complete key bytes atomically. Without force, hard-linking the
/// complete temporary file gives create-new semantics without a partial read.
pub fn write_client_device_key(
    key_dir: &Path,
    key_id: &str,
    bytes: &[u8],
    force: bool,
) -> io::Result<[u8; 32]> {
    validate_key_id(key_id)?;
    let raw = decode_key(bytes)?;
    let dir = key_dir.join("clients");
    fs::create_dir_all(&dir)?;
    perms::tighten_dir(&dir)?;
    let destination = dir.join(format!("{key_id}.pub"));
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let temp = dir.join(format!(
        ".{}.tmp",
        general_purpose::URL_SAFE_NO_PAD.encode(random)
    ));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        perms::tighten_file(&temp)?;
        file.write_all(&raw)?;
        file.sync_all()?;
        drop(file);
        if force {
            fs::rename(&temp, &destination)
        } else {
            fs::hard_link(&temp, &destination)
        }
        .map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                io::Error::new(
                    e.kind(),
                    format!(
                        "clients/{key_id}.pub already exists; re-run with --force to replace it"
                    ),
                )
            } else {
                e
            }
        })?;
        Ok(raw)
    })();
    let _ = fs::remove_file(temp);
    result
}

pub fn remove_client_device_key(key_dir: &Path, key_id: &str) -> io::Result<bool> {
    validate_key_id(key_id)?;
    match fs::remove_file(key_dir.join("clients").join(format!("{key_id}.pub"))) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trust_store_formats_permissions_replacement_and_removal() {
        let dir = std::env::temp_dir().join(format!("pan309-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let raw = [0xfbu8; 32];
        for (i, bytes) in [
            raw.to_vec(),
            general_purpose::STANDARD.encode(raw).into_bytes(),
            general_purpose::URL_SAFE_NO_PAD.encode(raw).into_bytes(),
        ]
        .iter()
        .enumerate()
        {
            write_client_device_key(&dir, &format!("k{i}"), bytes, false).unwrap();
            assert_eq!(
                fs::read(dir.join(format!("clients/k{i}.pub"))).unwrap(),
                raw
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.join("clients"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(dir.join("clients/k0.pub"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(write_client_device_key(&dir, "k0", &[2; 32], false).is_err());
        write_client_device_key(&dir, "k0", &[2; 32], true).unwrap();
        assert_eq!(fs::read(dir.join("clients/k0.pub")).unwrap(), [2; 32]);
        assert!(remove_client_device_key(&dir, "k0").unwrap());
        assert!(!remove_client_device_key(&dir, "k0").unwrap());
        for bad in ["", "../k", "bad/id", "k.pub", "é"] {
            assert!(write_client_device_key(&dir, bad, &raw, false).is_err());
            assert!(remove_client_device_key(&dir, bad).is_err());
        }
        for bytes in [vec![], vec![0; 31], vec![0; 33], b"not a key".to_vec()] {
            assert!(write_client_device_key(&dir, "bad", &bytes, false).is_err());
        }
        assert_eq!(
            crate::arm::DisarmReason::LocalTrustRevoked.as_str(),
            "local-trust-revoked"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
