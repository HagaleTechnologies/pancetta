use super::tests::fixture_app;
use super::*;
use std::time::{Duration, Instant};

fn band(app: &App) -> &str {
    &app.config.bands.bands[app.current_band_index].name
}

#[tokio::test]
async fn a_settled_readback_on_another_band_moves_band_and_dial() {
    let mut app = fixture_app().await;
    app.update_frequency(14_074_000);
    app.update_frequency_at(7_074_000, Instant::now() + Duration::from_secs(4));
    assert_eq!(band(&app), "40M");
    assert_eq!(app.station_info.operating_frequency, 7.074);
    assert_eq!(app.frequency_delta_khz(), Some(0.0));
    assert!(app.status_message().contains("Rig is on 7.074 MHz (40M)"));
}

#[tokio::test]
async fn following_the_rig_keeps_the_live_lists_and_the_band_cache() {
    let mut app = fixture_app().await;
    app.update_frequency(14_074_000);
    app.add_decoded_message(tests::fixture_view("KEEP", -8))
        .await
        .unwrap();
    let decoded_before = app.decoded_messages.len();
    app.dx_stations.insert(
        "KEEP".into(),
        tests::dx_fixture("KEEP", 10, 0, 14.074, false),
    );
    app.band_cache.insert(
        "17M".into(),
        BandSnapshot {
            decoded_messages: VecDeque::new(),
            dx_stations: Default::default(),
            captured_at: UtcTimestamp::now(),
        },
    );
    app.update_frequency_at(7_074_000, Instant::now() + Duration::from_secs(4));
    assert_eq!(app.decoded_messages.len(), decoded_before);
    assert!(app.dx_stations.contains_key("KEEP"));
    assert!(app.band_cache.contains_key("17M"));
}

#[tokio::test]
async fn a_band_key_holds_off_a_lagging_readback_for_the_settle_window() {
    let mut app = fixture_app().await;
    app.update_frequency(7_074_000);
    app.band_up();
    app.band_up();
    app.update_frequency_at(7_074_000, Instant::now() + Duration::from_secs(1));
    assert_eq!(band(&app), "20M");
}

#[tokio::test]
async fn a_readback_still_on_the_old_band_after_the_settle_window_is_followed() {
    let mut app = fixture_app().await;
    app.update_frequency(7_074_000);
    app.band_up();
    app.band_up();
    app.update_frequency_at(7_074_000, Instant::now() + Duration::from_secs(4));
    assert_eq!(band(&app), "40M");
}

#[tokio::test]
async fn a_readback_on_the_commanded_dial_is_never_held() {
    for manual in [false, true] {
        let mut app = fixture_app().await;
        app.update_frequency(14_074_000);
        if manual {
            app.apply_manual_qsy(7_074_000);
        } else {
            app.apply_spot_qsy(7_074_000);
        }
        app.update_frequency_at(7_074_000, Instant::now() + Duration::from_millis(500));
        assert_eq!(band(&app), "40M");
        assert_eq!(app.station_info.operating_frequency, 7.074);
        assert!(!app.status_message().contains("Rig is on 7.074"));
    }
}

#[tokio::test]
async fn a_readback_on_a_third_band_inside_the_window_is_held() {
    let mut app = fixture_app().await;
    app.update_frequency(14_074_000);
    app.apply_spot_qsy(7_074_000);
    app.update_frequency_at(21_074_000, Instant::now() + Duration::from_secs(1));
    assert_eq!(band(&app), "40M");
    app.update_frequency_at(21_074_000, Instant::now() + Duration::from_secs(4));
    assert_eq!(band(&app), "15M");
}

#[tokio::test]
async fn a_zero_readback_is_never_followed() {
    let mut app = fixture_app().await;
    app.update_frequency(14_074_000);
    app.update_frequency_at(0, Instant::now() + Duration::from_secs(4));
    assert_eq!(band(&app), "20M");
    assert_eq!(app.station_info.operating_frequency, 14.074);
}

#[tokio::test]
async fn a_rig_outside_the_configured_bands_is_followed_without_a_band_switch() {
    let mut app = fixture_app().await;
    app.update_frequency(14_074_000);
    app.update_frequency_at(474_200, Instant::now() + Duration::from_secs(4));
    assert_eq!(band(&app), "20M");
    assert_eq!(app.station_info.operating_frequency, 0.4742);
    assert_eq!(
        app.status_message(),
        "Rig is on 0.474 MHz (outside the configured bands)"
    );
}

#[tokio::test]
async fn band_keys_step_from_the_followed_band() {
    let mut app = fixture_app().await;
    app.update_frequency(14_074_000);
    app.update_frequency_at(7_074_000, Instant::now() + Duration::from_secs(4));
    assert_eq!(app.band_up(), 10_136_000);
}

#[tokio::test]
async fn a_frequency_modal_qsy_sets_the_dial_and_band_without_clearing_lists() {
    let mut app = fixture_app().await;
    app.update_frequency(14_074_000);
    app.add_decoded_message(tests::fixture_view("KEEP", -8))
        .await
        .unwrap();
    app.dx_stations.insert(
        "KEEP".into(),
        tests::dx_fixture("KEEP", 10, 0, 14.074, false),
    );
    let status_before = app.status_message();
    app.apply_manual_qsy(7_074_000);
    assert_eq!(band(&app), "40M");
    assert_eq!(app.station_info.operating_frequency, 7.074);
    assert!(app.dx_stations.contains_key("KEEP"));
    assert!(!app.decoded_messages.is_empty());
    assert_eq!(app.status_message(), status_before);
}

#[tokio::test]
async fn a_band_key_caches_only_its_band_after_qsy_kept_live_rows() {
    for manual in [false, true] {
        let mut app = fixture_app().await;
        app.update_frequency(14_074_000);
        let mut old = tests::fixture_view("N4BP", -8);
        old.frequency = 14.074;
        app.add_decoded_message(old).await.unwrap();

        if manual {
            app.apply_manual_qsy(7_074_000);
        }
        app.update_frequency(7_074_000);
        let mut current = tests::fixture_view("K1ABC", -8);
        current.frequency = 7.074;
        app.add_decoded_message(current).await.unwrap();
        assert_eq!(app.decoded_messages.len(), 2);
        assert_eq!(app.dx_stations.len(), 2);

        assert_eq!(app.band_up(), 10_136_000);
        assert_eq!(app.band_down(), 7_074_000);
        let restored: Vec<_> = app
            .decoded_messages
            .iter()
            .map(|row| row.frequency)
            .collect();
        assert_eq!(restored, [7.074]);
        assert_eq!(app.dx_stations.len(), 1);
        assert!(app.dx_stations.contains_key("K1ABC"));
    }
}
