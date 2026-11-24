// PAN-496. At launch the band selection is only the configured starting
// frequency (20m by default). The rig's first frequency report must replace
// it with where the radio really is, without retuning the radio, unless the
// operator or coordinator already changed band.
use super::tests::fixture_app;
use super::*;

fn band_index(app: &App, name: &str) -> usize {
    app.config
        .bands
        .bands
        .iter()
        .position(|b| b.name == name)
        .unwrap()
}

fn shown_band(app: &App) -> &str {
    &app.config.bands.bands[app.current_band_index].name
}

#[tokio::test]
async fn first_rig_report_adopts_the_rigs_band_and_dial() {
    let mut app = fixture_app().await;
    assert_eq!(
        shown_band(&app),
        "20M",
        "fixture starts on the config default"
    );

    app.update_frequency(18_100_000);

    assert_eq!(shown_band(&app), "17M");
    assert_eq!(app.station_info.operating_frequency, 18.1);
    assert_eq!(app.frequency_delta_khz(), Some(0.0), "no false delta");
}

#[tokio::test]
async fn first_rig_report_in_the_same_band_adopts_the_exact_dial() {
    let mut app = fixture_app().await;
    let twenty = band_index(&app, "20M");
    app.dx_stations.insert(
        "KEEP".into(),
        tests::dx_fixture("KEEP", 10, 0, 14.074, false),
    );

    app.update_frequency(14_080_000);

    assert_eq!(app.current_band_index, twenty);
    assert_eq!(app.station_info.operating_frequency, 14.08);
    assert!(
        app.dx_stations.contains_key("KEEP"),
        "same band keeps lists"
    );
}

#[tokio::test]
async fn later_rig_reports_only_move_the_delta() {
    let mut app = fixture_app().await;
    app.update_frequency(18_100_000);

    app.update_frequency(7_074_000);

    assert_eq!(shown_band(&app), "17M", "only the first report is adopted");
    assert_eq!(app.station_info.operating_frequency, 18.1);
    assert_eq!(app.radio_frequency, Some(7.074));
}

#[tokio::test]
async fn a_band_key_before_the_first_report_wins() {
    let mut app = fixture_app().await;
    app.band_down();
    let chosen = app.current_band_index;
    let chosen_mhz = app.station_info.operating_frequency;

    // The rig hasn't moved yet: its first poll still shows the old dial.
    app.update_frequency(18_100_000);

    assert_eq!(app.current_band_index, chosen);
    assert_eq!(app.station_info.operating_frequency, chosen_mhz);
}

#[tokio::test]
async fn a_spot_qsy_before_the_first_report_wins() {
    let mut app = fixture_app().await;
    app.apply_spot_qsy(14_076_000);

    app.update_frequency(18_100_000);

    assert_eq!(shown_band(&app), "20M");
    assert_eq!(app.station_info.operating_frequency, 14.076);
}

#[tokio::test]
async fn a_rig_outside_the_configured_bands_still_shows_its_dial() {
    let mut app = fixture_app().await;
    let start = app.current_band_index;

    app.update_frequency(474_200);

    assert_eq!(app.current_band_index, start, "no band to switch to");
    assert_eq!(app.station_info.operating_frequency, 0.4742);
    assert_eq!(app.frequency_delta_khz(), Some(0.0));
}
