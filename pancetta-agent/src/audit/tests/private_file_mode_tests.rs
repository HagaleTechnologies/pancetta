// PAN-365. A new audit log is created owner-only.
use super::tests::{ev, TempPath};
use super::*;

#[cfg(unix)]
#[test]
fn first_audit_entry_creates_owner_only_file() {
    use std::os::unix::fs::PermissionsExt;
    let tp = TempPath::new("private");
    let log = AuditLog::new(tp.path().clone());
    log.append(&ev(1000, AuditKind::Armed, Some("N0CALL"), "armed"))
        .unwrap();
    assert_eq!(
        std::fs::metadata(tp.path()).unwrap().permissions().mode() & 0o077,
        0
    );
}
