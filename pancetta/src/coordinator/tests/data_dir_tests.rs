//! PAN-376: station state resolves through the data directory.

use super::data_dir_for;

#[test]
fn data_dir_for_uses_the_test_home_or_the_process_data_dir() {
    // Under the test-only home override, the data directory is that home's
    // `.pancetta`, so in-process tests keep their isolated layout.
    let home = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        data_dir_for(Some(home.path())),
        Some(home.path().join(".pancetta"))
    );

    // Without an override it is the process data directory (this unit-test
    // binary never sets `--data-dir`, so that is `<home>/.pancetta`).
    assert_eq!(data_dir_for(None), pancetta_core::data_dir::try_current());
}
