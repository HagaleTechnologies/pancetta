// PAN-381. DX Hunter source merge for decodes a same-host WSJT-X sent over
// UDP: `EXT` rows, the native upgrade, no downgrade, and the network merge.
use super::tests::{fixture_app, fixture_view};
use super::*;

const ALL: [SpotSource; 5] = [
    SpotSource::Local,
    SpotSource::Network,
    SpotSource::Both,
    SpotSource::External,
    SpotSource::ExternalNetwork,
];

fn decode(call: &str, external: bool) -> DecodedMessageView {
    let mut v = fixture_view(call, -10);
    v.external = external;
    v
}

fn spot(call: &str) -> crate::tui_runner::CqdxSpotInfo {
    crate::tui_runner::CqdxSpotInfo {
        dx_call: call.to_string(),
        band: "20m".to_string(),
        mode: "FT8".to_string(),
        frequency_hz: 14_074_000,
        grid: None,
        rarity_tier: "common".to_string(),
        reporter_count: 2,
        best_snr: Some(-10),
        confidence: 0.9,
        first_seen: chrono::Utc::now().timestamp(),
        last_seen: chrono::Utc::now().timestamp(),
        is_notable: false,
        notable_type: None,
        entity_name: "United States".to_string(),
        needed: false,
        atno: false,
    }
}

/// Seed a row for `call` and force its source, then return the app.
async fn app_with_row(call: &str, source: SpotSource) -> App {
    let mut app = fixture_app().await;
    app.add_decoded_message(decode(call, false)).await.unwrap();
    app.dx_stations.get_mut(call).unwrap().source = source;
    app
}

#[tokio::test]
async fn external_decode_creates_ext_row() {
    let mut app = fixture_app().await;
    app.add_decoded_message(decode("N4BP", true)).await.unwrap();
    assert_eq!(app.dx_stations["N4BP"].source, SpotSource::External);
    // The external decode is also in Band Activity, still tagged.
    assert!(app.decoded_messages.back().unwrap().external);

    // A native decode of a new station still makes a plain `RX` row.
    app.add_decoded_message(decode("K1ABC", false))
        .await
        .unwrap();
    assert_eq!(app.dx_stations["K1ABC"].source, SpotSource::Local);
}

#[tokio::test]
async fn native_decode_upgrades_external_rows() {
    // Table column "native decode", every existing source.
    let expected = [
        (SpotSource::Local, SpotSource::Local),
        (SpotSource::Network, SpotSource::Both),
        (SpotSource::Both, SpotSource::Both),
        (SpotSource::External, SpotSource::Local),
        (SpotSource::ExternalNetwork, SpotSource::Both),
    ];
    for (existing, result) in expected {
        let mut app = app_with_row("N4BP", existing.clone()).await;
        app.add_decoded_message(decode("N4BP", false))
            .await
            .unwrap();
        assert_eq!(
            app.dx_stations["N4BP"].source, result,
            "{existing:?} + native decode"
        );
        assert_eq!(existing.clone().after_decode(false), result);
    }
}

#[tokio::test]
async fn external_decode_never_downgrades_local_rows() {
    // Table column "external decode", every existing source.
    let expected = [
        (SpotSource::Local, SpotSource::Local),
        (SpotSource::Network, SpotSource::ExternalNetwork),
        (SpotSource::Both, SpotSource::Both),
        (SpotSource::External, SpotSource::External),
        (SpotSource::ExternalNetwork, SpotSource::ExternalNetwork),
    ];
    for (existing, result) in expected {
        let mut app = app_with_row("N4BP", existing.clone()).await;
        app.add_decoded_message(decode("N4BP", true)).await.unwrap();
        assert_eq!(
            app.dx_stations["N4BP"].source, result,
            "{existing:?} + external decode"
        );
        assert_eq!(existing.clone().after_decode(true), result);
    }

    // A locally heard row keeps pancetta's own call target: an external
    // decode at another offset does not move it.
    for existing in [SpotSource::Local, SpotSource::Both] {
        let mut app = app_with_row("N4BP", existing.clone()).await;
        let before = app.dx_stations["N4BP"].audio_offset_hz;
        let mut ext = decode("N4BP", true);
        ext.delta_freq = 2_345.0;
        app.add_decoded_message(ext).await.unwrap();
        assert_eq!(
            app.dx_stations["N4BP"].audio_offset_hz, before,
            "{existing:?} row moved by an external decode"
        );
    }
}

#[tokio::test]
async fn network_spot_merges_with_external_rows() {
    // Table column "network spot", every existing source.
    let expected = [
        (SpotSource::Local, SpotSource::Both),
        (SpotSource::Network, SpotSource::Network),
        (SpotSource::Both, SpotSource::Both),
        (SpotSource::External, SpotSource::ExternalNetwork),
        (SpotSource::ExternalNetwork, SpotSource::ExternalNetwork),
    ];
    for (existing, result) in expected {
        let mut app = app_with_row("N4BP", existing.clone()).await;
        app.merge_spot_groups(&[spot("N4BP")]);
        assert_eq!(
            app.dx_stations["N4BP"].source, result,
            "{existing:?} + network spot"
        );
        assert_eq!(existing.clone().after_network_spot(), result);
    }

    // No row yet: a network spot makes a `NET` row.
    let mut app = fixture_app().await;
    app.merge_spot_groups(&[spot("P5DX")]);
    assert_eq!(app.dx_stations["P5DX"].source, SpotSource::Network);
}

#[test]
fn spot_source_labels() {
    let labels: Vec<String> = ALL.iter().map(ToString::to_string).collect();
    assert_eq!(labels, ["RX", "NET", "RX+N", "EXT", "EX+N"]);
}
