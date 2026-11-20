// PAN-365. Session state is saved owner-only and atomically; a symlink planted
// at the legacy fixed temp name is never followed (PAN-464 decision B).
use super::*;

#[cfg(unix)]
#[test]
fn save_creates_the_file_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private/session_state.json");
    save(&path, &SessionRecord::default()).unwrap();
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

/// PAN-464 decision B: a symlink planted at the legacy fixed temp name
/// (`session_state.json.tmp`) pointing OUTSIDE the state directory must
/// never be opened or followed. The save still succeeds through a fresh
/// exclusive temp sibling, the link's target is byte-for-byte unchanged,
/// the link itself is left in place, and the target is owner-only.
#[cfg(unix)]
#[test]
fn save_succeeds_without_following_a_planted_legacy_tmp_symlink() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = dir.path().join("session_state.json");
    let planted = path.with_extension("json.tmp");
    let sentinel = outside.path().join("sentinel.txt");
    std::fs::write(&sentinel, "untouched").unwrap();
    std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o644)).unwrap();
    symlink(&sentinel, &planted).unwrap();
    let record = SessionRecord {
        tx_freq_mode: Some(TxFreqMode::Auto),
        ..Default::default()
    };
    save(&path, &record).expect("save must succeed despite the planted symlink");
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "untouched");
    assert_eq!(
        std::fs::metadata(&sentinel).unwrap().permissions().mode() & 0o777,
        0o644,
        "sentinel mode must not change"
    );
    assert!(std::fs::symlink_metadata(&planted)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read_link(&planted).unwrap(), sentinel);
    let meta = std::fs::symlink_metadata(&path).unwrap();
    assert!(meta.file_type().is_file(), "target must be a regular file");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(load(&path), SessionLoad::Record(record));
}
