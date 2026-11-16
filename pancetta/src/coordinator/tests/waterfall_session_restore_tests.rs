// PAN-389. Coordinator startup restores the saved waterfall palette and gain
// for the TUI, and a newer `[ui.waterfall]` config edit supersedes the saved
// gain and rewrites the session file.
use super::tests::{build_coordinator_with_home, write_session_file};
use super::*;

/// PAN-389 AC15: a saved waterfall palette and gain whose baselines match
/// the config are restored and become the effective values the TUI
/// config is built from.
#[tokio::test]
async fn session_restore_seeds_the_waterfall_for_the_tui() {
    let home = tempfile::tempdir().unwrap();
    write_session_file(
        home.path(),
        r#"{"tx_freq_mode":null,"decode_effort":null,"decode_effort_config_at_save":null,"waterfall_palette":"classic","waterfall_palette_config_at_save":"viridis","waterfall_gain_db":8.0,"waterfall_gain_db_config_at_save":6.0}"#,
    );
    let mut config = Config::default();
    config.ui.waterfall.palette = pancetta_config::ui::WaterfallPalette::Viridis;
    config.ui.waterfall.gain_db = 6.0;

    let coordinator =
        build_coordinator_with_home(config.clone(), Some(home.path().to_path_buf())).await;

    let effective = session_state::effective_waterfall(
        &coordinator
            .session_record
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
        &config.ui.waterfall,
    );
    assert_eq!(
        effective,
        pancetta_config::ui::WaterfallDisplayConfig {
            palette: pancetta_config::ui::WaterfallPalette::Classic,
            gain_db: 8.0,
        }
    );
    assert_eq!(
        coordinator.session_report.status.as_deref(),
        Some("Session: restored waterfall palette classic and waterfall gain +8 dB from the last session")
    );
}

/// PAN-389 AC16: a config `gain_db` edit newer than the saved gain wins
/// at startup, the file is rewritten without the gain pair (the matching
/// palette pair is kept), and a second restart keeps the config gain.
#[tokio::test]
async fn session_config_edit_supersedes_saved_waterfall_gain_and_rewrites_the_file() {
    let home = tempfile::tempdir().unwrap();
    let path = write_session_file(
        home.path(),
        r#"{"tx_freq_mode":"AUTO","decode_effort":null,"decode_effort_config_at_save":null,"waterfall_palette":"classic","waterfall_palette_config_at_save":"viridis","waterfall_gain_db":8.0,"waterfall_gain_db_config_at_save":6.0}"#,
    );
    let mut config = Config::default();
    config.ui.waterfall.palette = pancetta_config::ui::WaterfallPalette::Viridis;
    config.ui.waterfall.gain_db = 3.0;

    for restart in ["first", "second"] {
        let coordinator =
            build_coordinator_with_home(config.clone(), Some(home.path().to_path_buf())).await;
        let effective = session_state::effective_waterfall(
            &coordinator
                .session_record
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
            &config.ui.waterfall,
        );
        assert_eq!(
            effective.gain_db, 3.0,
            "{restart} restart: the newer config gain must win over the saved 8 dB"
        );
        assert_eq!(
            effective.palette,
            pancetta_config::ui::WaterfallPalette::Classic,
            "{restart} restart: the matching saved palette is kept"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("waterfall_gain_db"),
            "{restart} restart: the superseded gain must be dropped from disk: {text}"
        );
        assert!(
            text.contains(
                r#""waterfall_palette":"classic","waterfall_palette_config_at_save":"viridis""#
            ),
            "{restart} restart: the palette pair must be kept: {text}"
        );
    }
}
