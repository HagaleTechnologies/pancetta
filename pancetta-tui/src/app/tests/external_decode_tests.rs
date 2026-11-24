// PAN-381. DX Hunter source merge for decodes a same-host WSJT-X sent over
// UDP: `EXT` rows, the native upgrade, no downgrade, and the network merge.
use super::tests::{fixture_app, fixture_view};
use super::*;
use pancetta_core::time::UtcTimestamp;

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
        first_seen: UtcTimestamp::now().as_second(),
        last_seen: UtcTimestamp::now().as_second(),
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

/// A network-spot row that WSJT-X then heard (`EX+N`) keeps the PAN-287
/// spot call: pancetta never heard it, so Space QSYs to the spot's dial
/// rather than calling at WSJT-X's offset on whatever dial the rig is on.
#[tokio::test]
async fn space_on_external_network_row_keeps_the_spot_call() {
    let mut app = app_with_row("N4BP", SpotSource::Network).await;
    {
        let row = app.dx_stations.get_mut("N4BP").unwrap();
        row.audio_offset_hz = None;
        row.spot_rf_hz = Some(14_075_210);
    }
    app.add_decoded_message(decode("N4BP", true)).await.unwrap();
    assert_eq!(app.dx_stations["N4BP"].source, SpotSource::ExternalNetwork);
    assert!(app.dx_stations["N4BP"].audio_offset_hz.is_some());
    app.active_panel = ActivePanel::DxHunter;
    app.dx_hunter_scroll = 0;
    match app.resolve_space_action() {
        Some(SpaceAction::CallSpot {
            callsign, dial_hz, ..
        }) => {
            assert_eq!(callsign, "N4BP");
            assert_eq!(dial_hz, 14_074_000);
        }
        other => panic!("expected the PAN-287 spot call, got {other:?}"),
    }

    // Once pancetta's own receiver hears it (`RX+N`), Space calls at the
    // heard offset on the current dial, as for any locally heard row.
    app.add_decoded_message(decode("N4BP", false))
        .await
        .unwrap();
    assert_eq!(app.dx_stations["N4BP"].source, SpotSource::Both);
    assert!(matches!(
        app.resolve_space_action(),
        Some(SpaceAction::Call { .. })
    ));
}

/// Codex review on PR #700: a QSO-outcome rescore names the origin of the
/// inputs it was scored from, and applies only to a row of that origin. A
/// row pancetta heard itself (`RX`, `RX+N`) takes only native rescores; an
/// `EXT` or `EX+N` row, which a band change may have rebuilt from WSJT-X
/// decodes, takes only WSJT-X rescores.
#[tokio::test]
async fn rank_refresh_applies_only_to_a_row_of_its_origin() {
    let factors = pancetta_qso::PriorityScorer::new(
        pancetta_qso::priority::PriorityWeights::default(),
        Box::new(pancetta_qso::priority::NullLookup),
    )
    .score_tiered_explained("N4BP", None, -10, 14_074_000.0)
    .factors;
    for (source, external) in [
        (SpotSource::Local, false),
        (SpotSource::Both, false),
        (SpotSource::External, true),
        (SpotSource::ExternalNetwork, true),
    ] {
        let mut app = app_with_row("N4BP", source.clone()).await;
        let before = app.dx_stations["N4BP"].local_rank.unwrap().score;

        app.refresh_local_rank_from("N4BP", before + 777, factors, !external);
        assert_eq!(
            app.dx_stations["N4BP"].local_rank.unwrap().score,
            before,
            "{source:?} row took a rescore of the other origin"
        );

        app.refresh_local_rank_from("N4BP", before + 777, factors, external);
        assert_eq!(
            app.dx_stations["N4BP"].local_rank.unwrap().score,
            before + 777,
            "{source:?} row ignored a rescore of its own origin"
        );
    }
}

/// A Band Activity line for `call` heard on `dial_mhz`, from WSJT-X when
/// `external` (PAN-434).
fn decode_on(call: &str, dial_mhz: f64, external: bool) -> DecodedMessageView {
    let mut v = decode(call, external);
    v.frequency = dial_mhz;
    v
}

/// PAN-434 ticket scenario 2: K1ABC is worked on 20m and WSJT-X has 40m
/// decodes of K1ABC in Band Activity. A QSO-outcome rescore marks only
/// K1ABC's 20m lines worked, whichever receiver heard them.
#[tokio::test]
async fn worked_mark_stays_on_the_rows_band() {
    let mut app = fixture_app().await;
    for line in [
        decode_on("K1ABC", 14.074, false),
        decode_on("K1ABC", 7.074, true),
        decode_on("K1ABC", 7.076, true),
        decode_on("K1ABC", 14.074, true),
        decode_on("W9XYZ", 14.074, true),
    ] {
        app.add_decoded_message(line).await.unwrap();
    }
    // The native decode made the row, and WSJT-X decodes never move it.
    assert_eq!(app.dx_stations["K1ABC"].source, SpotSource::Local);
    assert_eq!(app.dx_stations["K1ABC"].frequency, 14.074);

    let mut factors = pancetta_qso::PriorityScorer::new(
        pancetta_qso::priority::PriorityWeights::default(),
        Box::new(pancetta_qso::priority::NullLookup),
    )
    .score_tiered_explained("K1ABC", None, -10, 14_074_000.0)
    .factors;
    factors.worked_on_band = true;
    let score = app.dx_stations["K1ABC"].local_rank.unwrap().score;
    app.refresh_local_rank_from("K1ABC", score, factors, false);

    let marks: Vec<(&str, f64, bool, bool)> = app
        .decoded_messages
        .iter()
        .map(|m| {
            (
                m.call_sign.as_deref().unwrap_or_default(),
                m.frequency,
                m.external,
                m.worked_before,
            )
        })
        .collect();
    assert_eq!(
        marks,
        vec![
            ("K1ABC", 14.074, false, true),
            ("K1ABC", 7.074, true, false),
            ("K1ABC", 7.076, true, false),
            ("K1ABC", 14.074, true, true),
            ("W9XYZ", 14.074, true, false),
        ],
        "only K1ABC's 20m lines are marked worked"
    );
    assert!(app.dx_stations["K1ABC"].worked_before);
}

/// PAN-434: worked marks compare dials with `is_band_change`, and an unknown
/// dial matches (PAN-69's fail-open rule).
#[test]
fn same_band_dial_follows_is_band_change() {
    for (a, b) in [
        (14.074, 14.095),
        (0.0, 7.074),
        (f64::NAN, 7.074),
        (14_074_000.0, 14_074_000.0),
        (0.4742, 0.4742),
    ] {
        assert!(same_band_dial(a, b), "({a}, {b}) is one band");
    }
    for (a, b) in [(14.074, 7.074), (0.4742, 7.074)] {
        assert!(!same_band_dial(a, b), "({a}, {b}) are different bands");
    }
}
