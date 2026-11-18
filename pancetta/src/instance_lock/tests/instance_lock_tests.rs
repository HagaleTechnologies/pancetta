use super::*;

#[test]
fn a_second_lock_on_the_same_directory_names_the_holders_pid() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _held = InstanceLock::acquire(tmp.path()).expect("first acquire");

    match InstanceLock::acquire(tmp.path()) {
        Err(InstanceLockError::Held { pid, data_dir }) => {
            assert_eq!(pid, Some(std::process::id()));
            assert_eq!(data_dir, display_dir(tmp.path()));
        }
        other => panic!("expected Held, got {other:?}"),
    }
}

#[test]
fn the_refusal_reads_another_pancetta_is_running() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _held = InstanceLock::acquire(tmp.path()).expect("first acquire");

    let err = InstanceLock::acquire(tmp.path()).expect_err("second acquire refused");
    let text = err.to_string();
    let expected = format!(
        "another pancetta is running (pid {}) with data directory {}",
        std::process::id(),
        display_dir(tmp.path()).display()
    );
    assert!(text.starts_with(&expected), "{text}");
    assert!(
        text.ends_with("Stop it first, or give this station its own directory with --data-dir."),
        "{text}"
    );
}

#[test]
fn an_unreadable_pid_reports_pid_unknown() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _held = InstanceLock::acquire(tmp.path()).expect("first acquire");
    let pid_path = tmp.path().join(PID_FILE_NAME);

    std::fs::write(&pid_path, "not a pid\n").expect("garble pid");
    let garbled = InstanceLock::acquire(tmp.path()).expect_err("refused");
    assert!(matches!(garbled, InstanceLockError::Held { pid: None, .. }));
    assert!(garbled
        .to_string()
        .starts_with("another pancetta is running (pid unknown) with data directory"));

    std::fs::remove_file(&pid_path).expect("remove pid");
    let missing = InstanceLock::acquire(tmp.path()).expect_err("refused");
    assert!(matches!(missing, InstanceLockError::Held { pid: None, .. }));
}

#[test]
fn releasing_the_lock_lets_the_next_station_start() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let first = InstanceLock::acquire(tmp.path()).expect("first acquire");
    drop(first);
    let _second = InstanceLock::acquire(tmp.path()).expect("acquire after release");
    assert!(tmp.path().join(LOCK_FILE_NAME).exists());
}

#[cfg(unix)]
#[test]
fn a_dot_dot_spelling_is_named_by_its_canonical_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("station");
    let _held = InstanceLock::acquire(&dir).expect("first acquire");
    let err = InstanceLock::acquire(&dir.join("..").join("station")).expect_err("refused");
    let canonical = display_dir(&dir);
    assert!(
        matches!(&err, InstanceLockError::Held { data_dir, .. } if *data_dir == canonical),
        "{err}"
    );
}

#[test]
fn different_data_directories_do_not_conflict() {
    let a = tempfile::tempdir().expect("tempdir");
    let b = tempfile::tempdir().expect("tempdir");
    let _a = InstanceLock::acquire(a.path()).expect("acquire a");
    let _b = InstanceLock::acquire(b.path()).expect("acquire b");
}

#[test]
fn acquire_creates_a_missing_data_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("pancetta-40m").join("nested");
    let _held = InstanceLock::acquire(&dir).expect("acquire");
    assert!(dir.join(LOCK_FILE_NAME).is_file());
    assert_eq!(
        std::fs::read_to_string(dir.join(PID_FILE_NAME)).expect("pid file"),
        format!("{}\n", std::process::id())
    );
}

#[test]
fn a_stale_pid_file_without_a_lock_does_not_block() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // A killed station leaves its pid file and the (unlocked) lock file.
    std::fs::write(tmp.path().join(LOCK_FILE_NAME), "").expect("lock file");
    std::fs::write(tmp.path().join(PID_FILE_NAME), "999999\n").expect("stale pid");

    let _held = InstanceLock::acquire(tmp.path()).expect("stale pid does not block");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join(PID_FILE_NAME)).expect("pid file"),
        format!("{}\n", std::process::id())
    );
}
