// PAN-389. Formerly the inline module `pan389_tests`.
use super::*;
use crate::coordinator::session_state::{
    SessionChoiceUpdate, SessionRecord, WaterfallGainChoice, WaterfallPaletteChoice,
};
use pancetta_config::ui::{WaterfallDisplayConfig, WaterfallPalette};
use pancetta_tui::tui_runner::TuiCommand;

fn viridis_six() -> WaterfallDisplayConfig {
    WaterfallDisplayConfig {
        palette: WaterfallPalette::Viridis,
        gain_db: 6.0,
    }
}

#[test]
fn waterfall_commands_map_to_session_updates_with_running_baseline() {
    assert_eq!(
        waterfall_session_update(
            &TuiCommand::SetWaterfallGain { gain_db: 7.0 },
            &viridis_six()
        ),
        Some(SessionChoiceUpdate::WaterfallGain {
            choice: 7.0,
            config_at_save: 6.0,
        })
    );
    assert_eq!(
        waterfall_session_update(
            &TuiCommand::SetWaterfallPalette {
                palette: WaterfallPalette::Thermal
            },
            &viridis_six()
        ),
        Some(SessionChoiceUpdate::WaterfallPalette {
            choice: WaterfallPalette::Thermal,
            config_at_save: WaterfallPalette::Viridis,
        })
    );
    assert_eq!(
        waterfall_session_update(&TuiCommand::CycleDecodeEffort, &viridis_six()),
        None
    );
}

#[test]
fn tui_ui_config_carries_the_effective_waterfall() {
    let mut config = pancetta_config::Config::default();
    config.ui.waterfall = viridis_six();

    let ui = tui_ui_config(&config.ui, &SessionRecord::default());
    assert_eq!(ui.theme, pancetta_tui::ThemeName::Dark);
    assert!(ui.show_coordinates);
    assert_eq!(ui.waterfall, viridis_six());

    let record = SessionRecord {
        waterfall_palette: Some(WaterfallPaletteChoice {
            choice: WaterfallPalette::Classic,
            config_at_save: WaterfallPalette::Viridis,
        }),
        waterfall_gain_db: Some(WaterfallGainChoice {
            choice: 8.0,
            config_at_save: 6.0,
        }),
        ..Default::default()
    };
    let ui = tui_ui_config(&config.ui, &record);
    assert_eq!(ui.theme, pancetta_tui::ThemeName::Dark);
    assert!(ui.show_coordinates);
    assert_eq!(
        ui.waterfall,
        WaterfallDisplayConfig {
            palette: WaterfallPalette::Classic,
            gain_db: 8.0,
        }
    );
}
