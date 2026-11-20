// PAN-365. A re-persisted identity key is tightened to 0600 before any byte is written.
use super::tests::temp_dir;
use super::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
#[test]
fn re_persisted_key_is_tightened_before_writing() {
    let dir = temp_dir("repersist");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(IDENTITY_KEY_FILE);
    fs::write(&path, [0u8; 32]).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    write_key_file(&path, &[7u8; 32]).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read(&path).unwrap(), [7u8; 32]);
    fs::remove_dir_all(dir).unwrap();
}
