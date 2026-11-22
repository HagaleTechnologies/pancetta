//! PAN-360: the default config search never reads the working directory.
//!
//! This binary holds exactly one test, because it changes process-wide
//! state (`HOME`, `XDG_CONFIG_HOME`, every `PANCETTA_*` variable and the
//! current directory) that a sibling test running in parallel would see.

use pancetta_config::Config;
use std::fs;

#[test]
fn default_search_ignores_a_config_in_the_working_directory() {
    let tmp = tempfile::tempdir().unwrap();
    // Canonical, so the paths the loader builds from `HOME` compare equal.
    let root = fs::canonicalize(tmp.path()).unwrap();
    let home = root.join("home");
    let cwd = root.join("cwd");
    fs::create_dir_all(home.join(".pancetta")).unwrap();
    fs::create_dir_all(&cwd).unwrap();

    // Values a later file cannot override under the merge rules
    // (`SkipNone` fields), so they show whether the cwd file was read.
    fs::write(
        cwd.join("pancetta.toml"),
        "[station]\ncallsign = \"K1CWD\"\noperator_name = \"CWD-LEAK\"\n\
         [rig]\ndata_mode = \"PKTUSB\"\n",
    )
    .unwrap();
    // The built-in "defaults" source is named `defaults.toml`, a relative
    // path. It must never resolve against the cwd either.
    fs::write(
        cwd.join("defaults.toml"),
        "[station]\noperator_name = \"DEFAULTS-LEAK\"\n[rig]\ndata_mode = \"DATA-U\"\n",
    )
    .unwrap();
    let home_file = home.join(".pancetta").join("pancetta.toml");
    fs::write(
        &home_file,
        "[station]\ncallsign = \"K9HOME\"\ngrid_square = \"EN52\"\n",
    )
    .unwrap();

    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    let pancetta_vars: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("PANCETTA_"))
        .collect();
    for k in pancetta_vars {
        std::env::remove_var(k);
    }
    std::env::set_current_dir(&cwd).unwrap();

    let (config, report) = Config::load_default_unvalidated().unwrap();

    assert_eq!(report.files, vec![home_file], "files read");
    assert!(
        !report.searched.contains(&cwd),
        "the cwd must not be searched: {:?}",
        report.searched
    );
    assert_eq!(config.station.callsign, "K9HOME");
    assert_eq!(config.station.operator_name, None);
    assert_eq!(config.rig.data_mode, None);
}
