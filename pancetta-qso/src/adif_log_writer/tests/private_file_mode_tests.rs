// PAN-365. A new ADIF logbook (live open or import) is created owner-only.
use super::tests::imported_record;
use super::*;
use tempfile::tempdir;

#[cfg(unix)]
#[tokio::test]
async fn first_live_open_creates_owner_only_logbook() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir().unwrap();
    let path = dir.path().join("qsos.adi");
    let _writer = AdifLogWriter::open(&path).await.unwrap();
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o077,
        0
    );
}

#[cfg(unix)]
#[test]
fn first_import_creates_owner_only_logbook() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir().unwrap();
    let path = dir.path().join("qsos.adi");
    let block = prepare_import_block(&path, &AdifProcessor::new(), &[imported_record()]).unwrap();
    write_import_block(&path, &block).unwrap();
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o077,
        0
    );
}
