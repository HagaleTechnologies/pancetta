// PAN-365. A new TX log is created owner-only.
use super::tests::rec;
use super::*;

#[cfg(unix)]
#[test]
fn first_tx_log_batch_creates_owner_only_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(TX_LOG_FILE_NAME);
    append_batch(&path, RotationPolicy::default(), &[rec("CQ N0CALL FN20")]).unwrap();
    assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
}
