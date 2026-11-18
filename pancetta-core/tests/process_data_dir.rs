//! The process data directory is set once and read everywhere (PAN-376).
//!
//! This binary holds exactly one test because it sets the process-wide
//! `OnceLock`; a second test in the same binary would see its value.

use std::path::PathBuf;

use pancetta_core::data_dir;

#[test]
fn the_process_data_dir_is_set_once_and_read_everywhere() {
    let home = dirs::home_dir().or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    assert_eq!(
        data_dir::try_current(),
        data_dir::default_for_home(home.as_deref())
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("pancetta-40m");
    data_dir::set_process_data_dir(dir.clone()).expect("first set");
    assert_eq!(data_dir::current(), dir);
    assert_eq!(data_dir::try_current(), Some(dir.clone()));

    let other = tmp.path().join("other");
    assert_eq!(data_dir::set_process_data_dir(other.clone()), Err(other));
    assert_eq!(data_dir::current(), dir);
}
