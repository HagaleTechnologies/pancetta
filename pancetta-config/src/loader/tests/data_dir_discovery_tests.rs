//! PAN-376: `pancetta --data-dir DIR` without `--config` reads config only
//! from `DIR`.

use crate::Config;
use tempfile::TempDir;

fn write_config(dir: &std::path::Path, callsign: &str) {
    std::fs::write(
        dir.join("pancetta.toml"),
        format!("[station]\ncallsign = \"{callsign}\"\n"),
    )
    .expect("write config");
}

#[test]
fn load_from_dir_reads_only_that_directory() {
    let data_dir = TempDir::new().expect("tempdir");
    let decoy = TempDir::new().expect("tempdir");
    write_config(data_dir.path(), "K1ABC");
    write_config(decoy.path(), "W9DECOY");

    let (config, report) = Config::load_from_dir_unvalidated(data_dir.path()).expect("load");
    assert_eq!(config.station.callsign, "K1ABC");
    assert_eq!(report.files, vec![data_dir.path().join("pancetta.toml")]);

    let (config, _warnings) = Config::load_from_dir_with_warnings(data_dir.path()).expect("load");
    assert_eq!(config.station.callsign, "K1ABC");
}

#[test]
fn load_from_dir_reports_only_that_directory_as_searched() {
    let data_dir = TempDir::new().expect("tempdir");

    let (config, report) = Config::load_from_dir_unvalidated(data_dir.path()).expect("load");
    assert!(report.files.is_empty());
    assert_eq!(report.searched, vec![data_dir.path().to_path_buf()]);
    assert_eq!(config.station.callsign, Config::default().station.callsign);
}
