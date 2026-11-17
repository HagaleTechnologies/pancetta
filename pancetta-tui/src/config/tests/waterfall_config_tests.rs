// PAN-389. The TUI config defaults the waterfall to classic / 0 dB and
// accepts a config written before `ui.waterfall` existed.
use super::*;

/// PAN-389: the TUI config defaults the waterfall to classic / 0 dB and
/// still deserialises a config written before `ui.waterfall` existed.
#[test]
fn ui_config_defaults_waterfall_and_accepts_missing_table() {
    use pancetta_config::ui::{WaterfallDisplayConfig, WaterfallPalette};
    let config = Config::default();
    assert_eq!(config.ui.waterfall, WaterfallDisplayConfig::default());
    assert_eq!(config.ui.waterfall.palette, WaterfallPalette::Classic);
    assert_eq!(config.ui.waterfall.gain_db, 0.0);

    let mut value = serde_json::to_value(&config).unwrap();
    let ui = value["ui"].as_object_mut().unwrap();
    assert!(ui.remove("waterfall").is_some());
    let loaded: Config = serde_json::from_value(value).unwrap();
    assert_eq!(loaded.ui.waterfall.palette, WaterfallPalette::Classic);
    assert_eq!(loaded.ui.waterfall.gain_db, 0.0);
}
