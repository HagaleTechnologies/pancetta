// PAN-365. The tier cache is saved owner-only and atomically.
use super::tests::make_cache;
use super::*;
use tempfile::TempDir;

#[cfg(unix)]
#[test]
fn save_cache_is_private_and_ignores_planted_tmp_symlink() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tier_cache.json");
    let planted = path.with_extension("json.tmp");
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, "untouched").unwrap();
    symlink(&victim, &planted).unwrap();
    let cache = make_cache("fast");
    save_cache(&path, &cache);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(std::fs::read_to_string(victim).unwrap(), "untouched");
    assert!(std::fs::symlink_metadata(planted)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(load_cache(&path).unwrap().tier, cache.tier);
}
