//! PAN-387: `[ui] theme` selects the TUI palette; an unknown label is a
//! load warning (and falls back to dark) on every explicit-file load path.
use pancetta_config::ui::ThemeName;
use pancetta_config::Config;

const UNKNOWN_WARNING: &str =
    "Unknown ui.theme \"solarized\"; using the dark theme. Valid values: dark, light, high-contrast.";

fn write_config(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pancetta.toml");
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

fn theme_warnings(warnings: &[String]) -> Vec<&String> {
    warnings.iter().filter(|w| w.contains("ui.theme")).collect()
}

#[test]
fn light_theme_file_loads_without_warnings() {
    let (_dir, path) = write_config("[ui]\ntheme = \"light\"\n");

    let (config, warnings) = Config::load_from_file_with_warnings(&path).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(config.ui.theme_name(), ThemeName::Light);

    let (config, report) = Config::load_from_file_with_report(&path).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(config.ui.theme_name(), ThemeName::Light);
}

#[test]
fn unknown_theme_is_a_load_warning_on_every_file_path() {
    let (_dir, path) = write_config("[ui]\ntheme = \"solarized\"\n");

    let (config, warnings) = Config::load_from_file_with_warnings(&path).unwrap();
    assert_eq!(theme_warnings(&warnings), vec![UNKNOWN_WARNING]);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(config.ui.theme_name(), ThemeName::Dark);

    let (config, report) = Config::load_from_file_with_report(&path).unwrap();
    assert_eq!(theme_warnings(&report.warnings), vec![UNKNOWN_WARNING]);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert_eq!(config.ui.theme_name(), ThemeName::Dark);
}

#[test]
fn high_contrast_flag_in_a_file_wins_over_the_theme_label() {
    let (_dir, path) =
        write_config("[ui]\ntheme = \"light\"\n\n[ui.accessibility]\nhigh_contrast = true\n");
    let (config, warnings) = Config::load_from_file_with_warnings(&path).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(config.ui.theme_name(), ThemeName::HighContrast);
}
