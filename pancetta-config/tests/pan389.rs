//! PAN-389: `[ui.waterfall]` palette and display gain.
use pancetta_config::ui::WaterfallPalette;
use pancetta_config::Config;

fn load(toml: &str) -> Result<(Config, Vec<String>), String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pancetta.toml");
    std::fs::write(&path, toml).unwrap();
    Config::load_from_file_with_warnings(&path).map_err(|e| e.to_string())
}

#[test]
fn ticket_waterfall_table_loads_without_warnings() {
    let (config, warnings) =
        load("[ui.waterfall]\npalette = \"viridis\"\ngain_db = 6\n").expect("ticket TOML loads");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert_eq!(config.ui.waterfall.palette, WaterfallPalette::Viridis);
    assert_eq!(config.ui.waterfall.gain_db, 6.0);
    config.validate().expect("ticket config validates");
}

#[test]
fn omitted_table_defaults_to_classic_and_zero_gain() {
    let (config, warnings) = load("[ui]\ntheme = \"default\"\n").expect("loads");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert_eq!(config.ui.waterfall.palette, WaterfallPalette::Classic);
    assert_eq!(config.ui.waterfall.gain_db, 0.0);
    config.validate().expect("default config validates");
}

#[test]
fn gain_db_out_of_range_or_non_finite_fails_validation() {
    for gain in ["20", "-12.5", "nan"] {
        let (config, _) = load(&format!("[ui.waterfall]\ngain_db = {gain}\n"))
            .unwrap_or_else(|e| panic!("gain_db = {gain} should parse: {e}"));
        let err = config
            .validate()
            .expect_err(&format!("gain_db = {gain} must fail validation"))
            .to_string();
        assert!(err.contains("ui.waterfall.gain_db"), "{gain}: {err}");
    }
}

#[test]
fn boundary_gains_validate() {
    for gain in ["-12", "12"] {
        let (config, _) = load(&format!("[ui.waterfall]\ngain_db = {gain}\n")).unwrap();
        config
            .validate()
            .unwrap_or_else(|e| panic!("gain_db = {gain} should validate: {e}"));
    }
}

#[test]
fn unknown_palette_fails_load_listing_valid_names() {
    let err = load("[ui.waterfall]\npalette = \"inferno\"\n").expect_err("unknown palette");
    assert!(err.contains("unknown variant `inferno`"), "{err}");
    for name in ["classic", "spectrum", "thermal", "viridis"] {
        assert!(err.contains(name), "error should name {name}: {err}");
    }
}
