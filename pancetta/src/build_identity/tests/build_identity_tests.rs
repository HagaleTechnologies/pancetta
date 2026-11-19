// PAN-377.
use super::*;
#[test]
fn build_date_follows_source_date_epoch() {
    assert_eq!(build_date(Some("0"), 1700000000).unwrap(), "1970-01-01");
    assert_eq!(build_date(Some("1700000000"), 0).unwrap(), "2023-11-14");
    for value in [None, Some(""), Some("  ")] {
        assert_eq!(build_date(value, 0).unwrap(), "1970-01-01");
    }
}
#[test]
fn build_date_rejects_a_malformed_source_date_epoch() {
    for value in ["abc", "-1", "1.5"] {
        assert!(build_date(Some(value), 0)
            .unwrap_err()
            .contains("SOURCE_DATE_EPOCH"));
    }
}
#[test]
fn short_sha_accepts_only_a_hex_abbreviation() {
    assert_eq!(short_sha(Some("abcdef0")), "abcdef0");
    for value in [
        None,
        Some(""),
        Some("abc"),
        Some("abcdefg"),
        Some("ABCDEF0"),
    ] {
        assert_eq!(short_sha(value), "unknown");
    }
}
#[test]
fn rerun_paths_lists_only_existing_git_paths() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("HEAD"), "ref").unwrap();
    std::fs::create_dir_all(dir.path().join("refs/heads")).unwrap();
    assert_eq!(
        rerun_paths(dir.path(), |name| Some(name.to_owned())),
        vec![dir.path().join("HEAD"), dir.path().join("refs/heads")]
    );
    assert!(rerun_paths(dir.path(), |_| None).is_empty());
}
#[test]
fn git_command_ignores_inherited_repository_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = git_command(dir.path(), &["status"]);
    assert_eq!(cmd.get_current_dir(), Some(dir.path()));
    for name in GIT_LOCATION_VARS {
        assert!(cmd
            .get_envs()
            .any(|(key, value)| key == name && value.is_none()));
    }
}
