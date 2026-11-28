// PAN-435, PAN-443 (PAN-381). Space and the Callers reply refuse a decode
// heard only through a same-host WSJT-X unless WSJT-X heard it within 10 Hz of
// pancetta's dial (the rig readback when known, else the operating frequency)
// and in pancetta's mode, with a status line and no command. A WSJT-X decode
// on pancetta's own dial and mode is acted on, including on a band
// `pancetta_core::Band` cannot name.
//
// PAN-429: Shift+H (Hound engage) on a DX Hunter `EXT` or `EX+N` row follows
// the same rule and shows the same line. An `EX+N` row with a network spot
// dial keeps the PAN-287 spot call on Space, and Enter in DX Hunter only
// opens Why.
use super::key_tests::{key, key_shift, make_runner};
use super::*;
use crate::app::{ActivePanel, DecodedMessageView, SpotSource};
use pancetta_core::time::UtcTimestamp;

/// A decode a same-host WSJT-X sent, as the relay builds it: dial in MHz and
/// the mode WSJT-X named.
fn ext_decode(text: &str, dial_mhz: f64, mode: &str, directed: bool) -> DecodedMessageView {
    DecodedMessageView {
        timestamp: UtcTimestamp::now(),
        frequency: dial_mhz,
        mode: mode.to_string(),
        snr: -8,
        delta_time: 0.0,
        delta_freq: 1234.0,
        call_sign: Some("N4BP".to_string()),
        grid_square: None,
        message: text.to_string(),
        distance: None,
        bearing: None,
        slot_parity: Some(pancetta_core::slot::SlotParity::Even),
        is_directed_at_us: directed,
        worked_before: false,
        needed: false,
        atno: false,
        band_needed: false,
        priority_score: None,
        priority_factors: None,
        is_own_tx: false,
        external: true,
    }
}

/// Put pancetta on 20 m FT8, add a WSJT-X CQ from N4BP heard on `dial_mhz`
/// in `mode`, and focus `panel` on it.
async fn seed_cq(app: &Arc<RwLock<App>>, dial_mhz: f64, mode: &str, panel: ActivePanel) {
    seed_cq_at(app, 14.074, dial_mhz, mode, panel).await;
}

/// Put pancetta on `pancetta_mhz` FT8 with no rig readback, add a WSJT-X CQ
/// from N4BP heard on `dial_mhz` in `mode`, and focus `panel` on it.
async fn seed_cq_at(
    app: &Arc<RwLock<App>>,
    pancetta_mhz: f64,
    dial_mhz: f64,
    mode: &str,
    panel: ActivePanel,
) {
    let mut a = app.write().await;
    a.station_info.operating_frequency = pancetta_mhz;
    a.station_info.mode = "FT8".to_string();
    a.radio_frequency = None;
    a.add_decoded_message(ext_decode("CQ N4BP EM70", dial_mhz, mode, false))
        .await
        .unwrap();
    a.active_panel = panel;
    a.band_activity_scroll = 0;
    a.dx_hunter_scroll = 0;
    assert_eq!(a.focused_callsign().as_deref(), Some("N4BP"));
}

/// Put pancetta on 20 m FT8 and add a WSJT-X decode of N4BP calling us,
/// heard on `dial_mhz` in `mode`, focused in the Callers panel.
async fn seed_caller(app: &Arc<RwLock<App>>, dial_mhz: f64, mode: &str) {
    seed_caller_at(app, 14.074, dial_mhz, mode).await;
}

/// Put pancetta on `pancetta_mhz` FT8 with no rig readback and add a WSJT-X
/// decode of N4BP calling us, heard on `dial_mhz` in `mode`, focused in the
/// Callers panel.
async fn seed_caller_at(app: &Arc<RwLock<App>>, pancetta_mhz: f64, dial_mhz: f64, mode: &str) {
    let mut a = app.write().await;
    a.station_info.operating_frequency = pancetta_mhz;
    a.station_info.mode = "FT8".to_string();
    a.radio_frequency = None;
    let text = format!("{} N4BP EM70", a.station_info.call_sign);
    a.add_decoded_message(ext_decode(&text, dial_mhz, mode, true))
        .await
        .unwrap();
    a.active_panel = ActivePanel::Callers;
    a.callers_scroll = 0;
    assert_eq!(a.focused_callsign().as_deref(), Some("N4BP"));
}

/// Cross-band and other-mode cases with the full status line each sets. A
/// cross-band line names both bands, byte-identical to PAN-435; a same-band
/// line names both dials (PAN-443).
const BAND_OR_MODE_REFUSALS: [(f64, &str, &str); 3] = [
    (
        7.074,
        "FT8",
        "Can't work N4BP. WSJT-X heard it on 40m FT8. Pancetta is on 20m FT8.",
    ),
    (
        14.080,
        "FT4",
        "Can't work N4BP. WSJT-X heard it on 14.080 MHz FT4. Pancetta is on 14.074 MHz FT8.",
    ),
    (
        14.074,
        "FT4",
        "Can't work N4BP. WSJT-X heard it on 14.074 MHz FT4. Pancetta is on 14.074 MHz FT8.",
    ),
];

#[tokio::test]
async fn space_refuses_an_ext_row_from_another_band_or_mode() {
    for panel in [ActivePanel::BandActivity, ActivePanel::DxHunter] {
        for (dial_mhz, mode, refusal) in BAND_OR_MODE_REFUSALS {
            let (mut r, cmd_rx, app) = make_runner().await;
            seed_cq(&app, dial_mhz, mode, panel).await;
            r.handle_key_event(key(' ')).await.unwrap();
            assert!(
                cmd_rx.try_recv().is_err(),
                "{panel:?}: Space on an EXT row heard on {dial_mhz} {mode} must send nothing"
            );
            assert_eq!(app.read().await.status_message(), refusal, "{panel:?}");
        }
    }
}

/// PAN-443 scenario 1: WSJT-X on 14.090 and pancetta on 14.074 share 20m
/// FT8, but the station is not on pancetta's dial, so Space must not key
/// 1234 Hz there.
#[tokio::test]
async fn space_refuses_an_ext_row_from_another_dial_on_the_same_band() {
    for panel in [ActivePanel::BandActivity, ActivePanel::DxHunter] {
        let (mut r, cmd_rx, app) = make_runner().await;
        seed_cq_at(&app, 14.074, 14.090, "FT8", panel).await;
        r.handle_key_event(key(' ')).await.unwrap();
        assert!(
            cmd_rx.try_recv().is_err(),
            "{panel:?}: Space on an EXT row from 14.090 must send nothing"
        );
        assert_eq!(
            app.read().await.status_message(),
            "Can't work N4BP. WSJT-X heard it on 14.090 MHz FT8. Pancetta is on 14.074 MHz FT8.",
            "{panel:?}"
        );
    }
}

/// PAN-443 scenario 2: both programs on one dial of a band
/// `pancetta_core::Band` cannot name (4m, 630m, 2200m, custom). Space calls
/// the row with its audio offset and parity, with or without a readback.
#[tokio::test]
async fn space_calls_an_ext_row_on_a_shared_dial_band_cannot_name() {
    for dial_mhz in [70.154, 0.4742, 0.136, 40.680] {
        assert_eq!(
            pancetta_core::Band::from_frequency((dial_mhz * 1_000_000.0_f64).round() as u64),
            None,
            "{dial_mhz} must be a dial with no band name"
        );
        for readback in [None, Some(dial_mhz)] {
            for panel in [ActivePanel::BandActivity, ActivePanel::DxHunter] {
                let (mut r, cmd_rx, app) = make_runner().await;
                seed_cq_at(&app, dial_mhz, dial_mhz, "FT8", panel).await;
                app.write().await.radio_frequency = readback;
                r.handle_key_event(key(' ')).await.unwrap();
                let sent = cmd_rx.try_recv();
                assert!(
                    matches!(
                        sent,
                        Ok(TuiCommand::CallStation {
                            ref callsign,
                            frequency: 1234,
                            dx_parity: Some(pancetta_core::slot::SlotParity::Even),
                        }) if callsign == "N4BP"
                    ),
                    "{panel:?} {dial_mhz} readback {readback:?}: got {sent:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn space_calls_an_ext_row_on_pancettas_band_and_mode() {
    for panel in [ActivePanel::BandActivity, ActivePanel::DxHunter] {
        let (mut r, cmd_rx, app) = make_runner().await;
        seed_cq(&app, 14.074, "FT8", panel).await;
        r.handle_key_event(key(' ')).await.unwrap();
        assert!(
            matches!(
                cmd_rx.try_recv(),
                Ok(TuiCommand::CallStation { ref callsign, .. }) if callsign == "N4BP"
            ),
            "{panel:?}: Space on a same-band, same-mode EXT row calls it"
        );
    }
}

/// A rig readback that names another band also refuses: it lags a band key,
/// and the rig transmits on its own dial. The line names bands, as PAN-435
/// did. A readback on another dial of pancetta's band is covered by
/// `a_rig_readback_on_another_dial_refuses`.
#[tokio::test]
async fn a_rig_readback_on_another_band_refuses() {
    for (dial_mhz, heard) in [(7.074, "40m"), (14.074, "20m")] {
        let (mut r, cmd_rx, app) = make_runner().await;
        seed_cq(&app, dial_mhz, "FT8", ActivePanel::BandActivity).await;
        app.write().await.radio_frequency = Some(7.074);
        r.handle_key_event(key(' ')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err(), "{heard} decode, rig on 40m");
        assert_eq!(
            app.read().await.status_message(),
            format!(
                "Can't work N4BP. WSJT-X heard it on {heard} FT8. Pancetta is on 20m FT8 \
                 (rig reads 40m)."
            )
        );
    }
}

/// PAN-443: the rig-lag guard sees a band change on a band
/// `pancetta_core::Band` cannot name, by the coordinator's 100 kHz rule.
#[tokio::test]
async fn a_rig_readback_a_band_away_on_a_band_without_a_name_refuses() {
    for (pancetta_mhz, readback, dial_mhz, refusal) in [
        (
            14.074,
            70.154,
            70.154,
            "Can't work N4BP. WSJT-X heard it on 70.154 MHz FT8. Pancetta is on 20m FT8 \
             (rig reads 70.154 MHz).",
        ),
        (
            70.154,
            14.074,
            14.074,
            "Can't work N4BP. WSJT-X heard it on 20m FT8. Pancetta is on 70.154 MHz FT8 \
             (rig reads 20m).",
        ),
    ] {
        let (mut r, cmd_rx, app) = make_runner().await;
        seed_cq_at(
            &app,
            pancetta_mhz,
            dial_mhz,
            "FT8",
            ActivePanel::BandActivity,
        )
        .await;
        app.write().await.radio_frequency = Some(readback);
        r.handle_key_event(key(' ')).await.unwrap();
        assert!(
            cmd_rx.try_recv().is_err(),
            "operating {pancetta_mhz}, rig {readback}"
        );
        assert_eq!(app.read().await.status_message(), refusal);
    }
}

/// PAN-443: pancetta's dial is the rig readback when known, compared within
/// 10 Hz of WSJT-X's.
#[tokio::test]
async fn a_rig_readback_on_another_dial_refuses() {
    for readback in [14.07401, 14.07399] {
        let (mut r, cmd_rx, app) = make_runner().await;
        seed_cq(&app, 14.074, "FT8", ActivePanel::BandActivity).await;
        app.write().await.radio_frequency = Some(readback);
        r.handle_key_event(key(' ')).await.unwrap();
        assert!(
            matches!(
                cmd_rx.try_recv(),
                Ok(TuiCommand::CallStation { ref callsign, .. }) if callsign == "N4BP"
            ),
            "readback {readback} is within 10 Hz of 14.074"
        );
    }

    for (readback, refusal) in [
        (
            14.074011,
            "Can't work N4BP. WSJT-X heard it on 14.074 MHz FT8. Pancetta is on 14.074011 MHz FT8.",
        ),
        (
            14.0745,
            "Can't work N4BP. WSJT-X heard it on 14.074 MHz FT8. Pancetta is on 14.0745 MHz FT8.",
        ),
    ] {
        let (mut r, cmd_rx, app) = make_runner().await;
        seed_cq(&app, 14.074, "FT8", ActivePanel::BandActivity).await;
        app.write().await.radio_frequency = Some(readback);
        r.handle_key_event(key(' ')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err(), "readback {readback}");
        assert_eq!(app.read().await.status_message(), refusal);
    }
}

/// PAN-443: after a manual in-band QSY the readback (14.090) is where the
/// rig keys, even though the operating frequency still reads 14.074.
#[tokio::test]
async fn the_rig_readback_is_pancettas_dial_after_a_manual_qsy() {
    let (mut r, cmd_rx, app) = make_runner().await;
    seed_cq(&app, 14.090, "FT8", ActivePanel::BandActivity).await;
    app.write().await.radio_frequency = Some(14.090);
    r.handle_key_event(key(' ')).await.unwrap();
    assert!(matches!(
        cmd_rx.try_recv(),
        Ok(TuiCommand::CallStation { ref callsign, frequency: 1234, .. }) if callsign == "N4BP"
    ));

    let (mut r, cmd_rx, app) = make_runner().await;
    seed_cq(&app, 14.074, "FT8", ActivePanel::BandActivity).await;
    app.write().await.radio_frequency = Some(14.090);
    r.handle_key_event(key(' ')).await.unwrap();
    assert!(cmd_rx.try_recv().is_err());
    assert_eq!(
        app.read().await.status_message(),
        "Can't work N4BP. WSJT-X heard it on 14.074 MHz FT8. Pancetta is on 14.090 MHz FT8."
    );
}

/// A WSJT-X decode from another band that called us does not block Space on
/// a row pancetta heard itself: that row is called as usual.
#[tokio::test]
async fn an_off_band_ext_caller_does_not_block_a_native_row() {
    let (mut r, cmd_rx, app) = make_runner().await;
    {
        let mut a = app.write().await;
        a.station_info.operating_frequency = 14.074;
        a.station_info.mode = "FT8".to_string();
        a.radio_frequency = None;
        let text = format!("{} N4BP EM70", a.station_info.call_sign);
        a.add_decoded_message(ext_decode(&text, 7.074, "FT8", true))
            .await
            .unwrap();
        let mut native = ext_decode("CQ N4BP EM70", 14.074, "FT8", false);
        native.external = false;
        a.add_decoded_message(native).await.unwrap();
        a.active_panel = ActivePanel::DxHunter;
        a.dx_hunter_scroll = 0;
        assert_eq!(a.focused_callsign().as_deref(), Some("N4BP"));
    }
    r.handle_key_event(key(' ')).await.unwrap();
    assert!(matches!(
        cmd_rx.try_recv(),
        Ok(TuiCommand::CallStation { ref callsign, .. }) if callsign == "N4BP"
    ));
}

#[tokio::test]
async fn directed_replies_refuse_an_ext_caller_from_another_band_or_mode() {
    for (dial_mhz, mode, refusal) in BAND_OR_MODE_REFUSALS {
        // Space takes the reply branch for a decode directed at us.
        let (mut r, cmd_rx, app) = make_runner().await;
        seed_caller(&app, dial_mhz, mode).await;
        r.handle_key_event(key(' ')).await.unwrap();
        assert!(
            cmd_rx.try_recv().is_err(),
            "Space reply to {dial_mhz} {mode}"
        );
        assert_eq!(app.read().await.status_message(), refusal);

        // Enter on the Callers panel replies to the selected caller.
        let (mut r, cmd_rx, app) = make_runner().await;
        seed_caller(&app, dial_mhz, mode).await;
        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(
            cmd_rx.try_recv().is_err(),
            "Callers Enter reply to {dial_mhz} {mode}"
        );
        assert_eq!(app.read().await.status_message(), refusal);
    }
}

/// PAN-443 scenario 1 for a decode directed at us: Space and Callers Enter
/// refuse a 14.090 caller while pancetta is on 14.074.
#[tokio::test]
async fn directed_replies_refuse_an_ext_caller_from_another_dial_on_the_same_band() {
    let refusal =
        "Can't work N4BP. WSJT-X heard it on 14.090 MHz FT8. Pancetta is on 14.074 MHz FT8.";

    let (mut r, cmd_rx, app) = make_runner().await;
    seed_caller_at(&app, 14.074, 14.090, "FT8").await;
    r.handle_key_event(key(' ')).await.unwrap();
    assert!(cmd_rx.try_recv().is_err(), "Space reply to 14.090");
    assert_eq!(app.read().await.status_message(), refusal);

    let (mut r, cmd_rx, app) = make_runner().await;
    seed_caller_at(&app, 14.074, 14.090, "FT8").await;
    r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(cmd_rx.try_recv().is_err(), "Callers Enter reply to 14.090");
    assert_eq!(app.read().await.status_message(), refusal);
}

/// PAN-443 scenario 2 for a decode directed at us: a shared dial with no band
/// name answers the caller.
#[tokio::test]
async fn directed_replies_answer_an_ext_caller_on_a_shared_dial_band_cannot_name() {
    for dial_mhz in [70.154, 0.4742, 0.136, 40.680] {
        let (mut r, cmd_rx, app) = make_runner().await;
        seed_caller_at(&app, dial_mhz, dial_mhz, "FT8").await;
        r.handle_key_event(key(' ')).await.unwrap();
        assert!(
            matches!(
                cmd_rx.try_recv(),
                Ok(TuiCommand::RespondToCaller { ref callsign, .. }) if callsign == "N4BP"
            ),
            "Space reply on {dial_mhz}"
        );

        let (mut r, cmd_rx, app) = make_runner().await;
        seed_caller_at(&app, dial_mhz, dial_mhz, "FT8").await;
        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(
            matches!(
                cmd_rx.try_recv(),
                Ok(TuiCommand::RespondToCaller { ref callsign, .. }) if callsign == "N4BP"
            ),
            "Callers Enter reply on {dial_mhz}"
        );
    }
}

#[tokio::test]
async fn space_refuses_the_selected_ext_caller_even_with_an_older_native_reply_target() {
    let (mut r, cmd_rx, app) = make_runner().await;
    seed_caller(&app, 14.074, "FT8").await;
    {
        let mut a = app.write().await;
        a.decoded_messages.back_mut().unwrap().external = false;
    }
    seed_caller(&app, 7.074, "FT8").await;
    assert!(app.read().await.selected_caller().unwrap().external);

    r.handle_key_event(key(' ')).await.unwrap();

    assert!(cmd_rx.try_recv().is_err());
    assert_eq!(
        app.read().await.status_message(),
        "Can't work N4BP. WSJT-X heard it on 40m FT8. Pancetta is on 20m FT8."
    );
}

#[tokio::test]
async fn directed_replies_answer_an_ext_caller_on_pancettas_band_and_mode() {
    let (mut r, cmd_rx, app) = make_runner().await;
    seed_caller(&app, 14.074, "FT8").await;
    r.handle_key_event(key(' ')).await.unwrap();
    assert!(matches!(
        cmd_rx.try_recv(),
        Ok(TuiCommand::RespondToCaller { ref callsign, .. }) if callsign == "N4BP"
    ));

    let (mut r, cmd_rx, app) = make_runner().await;
    seed_caller(&app, 14.074, "FT8").await;
    r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        cmd_rx.try_recv(),
        Ok(TuiCommand::RespondToCaller { ref callsign, .. }) if callsign == "N4BP"
    ));
}

/// A cqdx.io network spot of `call` on `rf_hz`, the same values as
/// `external_decode_tests.rs`'s `spot`.
fn cqdx_spot(call: &str, rf_hz: u64) -> crate::tui_runner::CqdxSpotInfo {
    crate::tui_runner::CqdxSpotInfo {
        dx_call: call.to_string(),
        band: "20m".to_string(),
        mode: "FT8".to_string(),
        frequency_hz: rf_hz,
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

async fn seed_net(app: &Arc<RwLock<App>>, rf_hz: u64) {
    let mut a = app.write().await;
    a.station_info.operating_frequency = 14.074;
    a.station_info.mode = "FT8".to_string();
    a.radio_frequency = None;
    a.merge_spot_groups(&[cqdx_spot("N4BP", rf_hz)]);
    a.active_panel = ActivePanel::DxHunter;
    a.dx_hunter_scroll = 0;
    assert_eq!(
        a.selected_dx_hunter_row().unwrap().source,
        SpotSource::Network
    );
}

#[tokio::test]
async fn shift_h_refuses_a_cross_band_net_spot() {
    let (mut r, cmd_rx, app) = make_runner().await;
    seed_net(&app, 7_075_210).await;
    r.handle_key_event(key_shift('H')).await.unwrap();
    assert!(cmd_rx.try_recv().is_err());
    assert_eq!(
        app.read().await.status_message(),
        "Hound: tune to 7.074 MHz FT8 for N4BP."
    );
}

#[tokio::test]
async fn shift_h_engages_a_net_spot_on_the_current_dial() {
    for readback in [None, Some(14.074_010)] {
        let (mut r, cmd_rx, app) = make_runner().await;
        seed_net(&app, 14_075_210).await;
        app.write().await.radio_frequency = readback;
        r.handle_key_event(key_shift('H')).await.unwrap();
        assert!(
            matches!(cmd_rx.try_recv(), Ok(TuiCommand::EngageHound { callsign, fox_freq: 1210, dx_parity: None, fox_grid: None }) if callsign == "N4BP")
        );
        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(
            app.read().await.status_message(),
            "Hound: engaging N4BP as Fox..."
        );
    }
}

#[tokio::test]
async fn shift_h_refuses_a_net_spot_off_the_rig_dial() {
    let (mut r, cmd_rx, app) = make_runner().await;
    seed_net(&app, 14_075_210).await;
    app.write().await.radio_frequency = Some(14.074_011);
    r.handle_key_event(key_shift('H')).await.unwrap();
    assert!(cmd_rx.try_recv().is_err());
    assert_eq!(
        app.read().await.status_message(),
        "Hound: tune to 14.074 MHz FT8 for N4BP."
    );
}

#[tokio::test]
async fn shift_h_refuses_a_net_spot_during_a_band_change() {
    let (mut r, cmd_rx, app) = make_runner().await;
    seed_net(&app, 7_075_210).await;
    app.write().await.radio_frequency = Some(7.074);
    r.handle_key_event(key_shift('H')).await.unwrap();
    assert!(cmd_rx.try_recv().is_err());
    assert_eq!(
        app.read().await.status_message(),
        "Hound: rig and operating bands differ."
    );
}

#[tokio::test]
async fn shift_h_refuses_a_net_spot_without_a_confirmed_offset() {
    for (rf_hz, missing_rf, unknown_dial, warning) in [
        (
            14_075_210,
            true,
            false,
            "Hound: no usable FT8 spot frequency for N4BP.",
        ),
        (
            14_025_000,
            false,
            false,
            "Hound: no usable FT8 spot frequency for N4BP.",
        ),
        (
            14_074_000,
            false,
            false,
            "Hound: no usable FT8 spot frequency for N4BP.",
        ),
        (
            14_075_210,
            false,
            true,
            "Hound: pancetta's dial is unknown.",
        ),
    ] {
        let (mut r, cmd_rx, app) = make_runner().await;
        seed_net(&app, rf_hz).await;
        {
            let mut a = app.write().await;
            if missing_rf {
                a.dx_stations.get_mut("N4BP").unwrap().spot_rf_hz = None;
            }
            if unknown_dial {
                a.station_info.operating_frequency = 0.0;
            }
        }
        r.handle_key_event(key_shift('H')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(app.read().await.status_message(), warning);
    }
}

#[tokio::test]
async fn space_on_a_cross_band_net_spot_sends_call_network_spot() {
    let (mut r, cmd_rx, app) = make_runner().await;
    seed_net(&app, 7_075_210).await;
    r.handle_key_event(key(' ')).await.unwrap();
    assert!(
        matches!(cmd_rx.try_recv(), Ok(TuiCommand::CallNetworkSpot { callsign, dial_hz: 7_074_000, audio_offset_hz: Some(1210), .. }) if callsign == "N4BP")
    );
    assert!(cmd_rx.try_recv().is_err());
}

/// Put pancetta on `pancetta_mhz` FT8 with no rig readback, merge a cqdx.io
/// spot of N4BP at 14 075 210 Hz, then add a WSJT-X CQ from N4BP heard on
/// `dial_mhz` in `mode`. The row's `spot_rf_hz` is then set to `spot_rf_hz`,
/// and DX Hunter is focused on the resulting `EX+N` row.
async fn seed_ex_n_at(
    app: &Arc<RwLock<App>>,
    pancetta_mhz: f64,
    spot_rf_hz: Option<u64>,
    dial_mhz: f64,
    mode: &str,
) {
    let mut a = app.write().await;
    a.station_info.operating_frequency = pancetta_mhz;
    a.station_info.mode = "FT8".to_string();
    a.radio_frequency = None;
    a.merge_spot_groups(&[cqdx_spot("N4BP", 14_075_210)]);
    a.add_decoded_message(ext_decode("CQ N4BP EM70", dial_mhz, mode, false))
        .await
        .unwrap();
    let row = a.dx_stations.get_mut("N4BP").unwrap();
    row.spot_rf_hz = spot_rf_hz;
    assert_eq!(row.source, SpotSource::ExternalNetwork);
    a.active_panel = ActivePanel::DxHunter;
    a.dx_hunter_scroll = 0;
    assert_eq!(a.focused_callsign().as_deref(), Some("N4BP"));
}

/// PAN-429 AC5: Shift+H on an `EXT` row, and on an `EX+N` row with a network
/// spot dial, heard by WSJT-X off pancetta's dial or mode, sends nothing and
/// sets the line Space sets for that row.
#[tokio::test]
async fn shift_h_refuses_an_ext_or_ex_n_row_off_pancettas_dial() {
    let mut cases = BAND_OR_MODE_REFUSALS.to_vec();
    cases.push((
        14.090,
        "FT8",
        "Can't work N4BP. WSJT-X heard it on 14.090 MHz FT8. Pancetta is on 14.074 MHz FT8.",
    ));
    for (dial_mhz, mode, refusal) in cases {
        for ex_n in [false, true] {
            let (mut r, cmd_rx, app) = make_runner().await;
            if ex_n {
                seed_ex_n_at(&app, 14.074, Some(14_075_210), dial_mhz, mode).await;
            } else {
                seed_cq_at(&app, 14.074, dial_mhz, mode, ActivePanel::DxHunter).await;
            }
            r.handle_key_event(key_shift('H')).await.unwrap();
            let sent = cmd_rx.try_recv();
            assert!(
                sent.is_err(),
                "EX+N {ex_n}: Shift+H on a row heard on {dial_mhz} {mode} must send nothing, \
                 got {sent:?}"
            );
            assert_eq!(
                app.read().await.status_message(),
                refusal,
                "EX+N {ex_n}: {dial_mhz} {mode}"
            );
        }
    }
}

/// PAN-429 AC5: a rig readback on another band refuses Shift+H on an `EXT`
/// row even when WSJT-X and the operating frequency share a dial.
#[tokio::test]
async fn shift_h_refuses_an_ext_row_while_the_rig_reads_another_band() {
    let (mut r, cmd_rx, app) = make_runner().await;
    seed_cq_at(&app, 14.074, 14.074, "FT8", ActivePanel::DxHunter).await;
    app.write().await.radio_frequency = Some(7.074);
    r.handle_key_event(key_shift('H')).await.unwrap();
    assert!(cmd_rx.try_recv().is_err());
    assert_eq!(
        app.read().await.status_message(),
        "Can't work N4BP. WSJT-X heard it on 20m FT8. Pancetta is on 20m FT8 (rig reads 40m)."
    );
}

/// PAN-429 AC6: on pancetta's dial and mode, Shift+H engages an `EXT` or
/// `EX+N` row at WSJT-X's offset and parity, as before.
#[tokio::test]
async fn shift_h_engages_an_ext_or_ex_n_row_on_pancettas_dial() {
    for ex_n in [false, true] {
        let (mut r, cmd_rx, app) = make_runner().await;
        if ex_n {
            seed_ex_n_at(&app, 14.074, Some(14_075_210), 14.074, "FT8").await;
        } else {
            seed_cq_at(&app, 14.074, 14.074, "FT8", ActivePanel::DxHunter).await;
        }
        r.handle_key_event(key_shift('H')).await.unwrap();
        match cmd_rx.try_recv() {
            Ok(TuiCommand::EngageHound {
                callsign,
                fox_freq,
                dx_parity,
                fox_grid,
            }) => {
                assert_eq!(callsign, "N4BP", "EX+N {ex_n}");
                assert_eq!(fox_freq, 1234, "EX+N {ex_n}");
                assert_eq!(
                    dx_parity,
                    Some(pancetta_core::slot::SlotParity::Even),
                    "EX+N {ex_n}"
                );
                assert_eq!(fox_grid, None, "EX+N {ex_n}");
            }
            other => panic!("EX+N {ex_n}: expected EngageHound, got {other:?}"),
        }
        assert!(cmd_rx.try_recv().is_err(), "EX+N {ex_n}: one command");
        assert_eq!(
            app.read().await.status_message(),
            "Hound: engaging N4BP as Fox...",
            "EX+N {ex_n}"
        );
    }
}

/// PAN-429 AC2: a network-spotted row that WSJT-X then hears on 40m (`EX+N`)
/// keeps the PAN-287 spot call on Space. Pancetta retunes to the spot's dial
/// at the spot's offset and never calls at WSJT-X's 1234 Hz.
#[tokio::test]
async fn space_on_a_cross_band_ex_n_row_calls_the_network_spot() {
    let (mut r, cmd_rx, app) = make_runner().await;
    seed_ex_n_at(&app, 14.074, Some(14_075_210), 7.074, "FT8").await;
    r.handle_key_event(key(' ')).await.unwrap();
    match cmd_rx.try_recv() {
        Ok(TuiCommand::CallNetworkSpot {
            callsign,
            band,
            dial_hz,
            audio_offset_hz,
        }) => {
            assert_eq!(callsign, "N4BP");
            assert_eq!(band, "20M");
            assert_eq!(dial_hz, 14_074_000);
            assert_eq!(audio_offset_hz, Some(1210));
        }
        other => panic!("expected the PAN-287 spot call, got {other:?}"),
    }
    let next = cmd_rx.try_recv();
    assert!(
        next.is_err(),
        "Space sends exactly one command, got {next:?}"
    );
}

/// PAN-429 AC3: an `EX+N` row with no network spot dial, heard by WSJT-X on
/// 40m while pancetta is on 20m, is refused like an `EXT` row.
#[tokio::test]
async fn space_refuses_a_cross_band_ex_n_row_with_no_spot_dial() {
    let (mut r, cmd_rx, app) = make_runner().await;
    seed_ex_n_at(&app, 14.074, None, 7.074, "FT8").await;
    r.handle_key_event(key(' ')).await.unwrap();
    assert!(cmd_rx.try_recv().is_err());
    assert_eq!(
        app.read().await.status_message(),
        "Can't work N4BP. WSJT-X heard it on 40m FT8. Pancetta is on 20m FT8."
    );
}

/// PAN-429 AC4: Enter in DX Hunter opens Why and never calls, for an `EXT`
/// row and an `EX+N` row heard by WSJT-X on another band.
#[tokio::test]
async fn enter_on_a_cross_band_ext_or_ex_n_dx_row_opens_why_and_sends_nothing() {
    for ex_n in [false, true] {
        let (mut r, cmd_rx, app) = make_runner().await;
        if ex_n {
            seed_ex_n_at(&app, 14.074, Some(14_075_210), 7.074, "FT8").await;
        } else {
            seed_cq_at(&app, 14.074, 7.074, "FT8", ActivePanel::DxHunter).await;
        }
        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        let sent = cmd_rx.try_recv();
        assert!(sent.is_err(), "EX+N {ex_n}: Enter sent {sent:?}");
        assert!(app.read().await.show_why, "EX+N {ex_n}: Enter opens Why");
    }
}

// PAN-475: keep the current readback when inserting a decode after a QSY.
async fn seed_cq_keeping_readback(app: &Arc<RwLock<App>>, dial_mhz: f64, panel: ActivePanel) {
    let mut a = app.write().await;
    a.add_decoded_message(ext_decode("CQ N4BP EM70", dial_mhz, "FT8", false))
        .await
        .unwrap();
    a.active_panel = panel;
    a.band_activity_scroll = 0;
    a.dx_hunter_scroll = 0;
    assert_eq!(a.focused_callsign().as_deref(), Some("N4BP"));
}

async fn submit_freq_modal(r: &mut TuiRunner, dial: &str) {
    r.handle_key_event(key_shift('F')).await.unwrap();
    for c in dial.chars() {
        r.handle_key_event(key(c)).await.unwrap();
    }
    r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
}

async fn band_keys_to_20m(r: &mut TuiRunner) {
    r.handle_key_event(key('-')).await.unwrap();
    r.handle_key_event(key('=')).await.unwrap();
}

#[tokio::test]
async fn space_calls_a_same_dial_ext_row_after_a_frequency_modal_qsy_to_another_band() {
    for panel in [ActivePanel::BandActivity, ActivePanel::DxHunter] {
        let (mut r, cmd_rx, app) = make_runner().await;
        band_keys_to_20m(&mut r).await;
        while cmd_rx.try_recv().is_ok() {}
        submit_freq_modal(&mut r, "7.074").await;
        while cmd_rx.try_recv().is_ok() {}
        assert_eq!(
            app.read().await.status_message(),
            "Dial 7.074 MHz (simplex)"
        );
        r.handle_message(TuiMessage::FrequencyUpdate {
            vfo: 0,
            frequency: 7_074_000,
        })
        .await
        .unwrap();
        assert_eq!(
            app.read().await.status_message(),
            "Dial 7.074 MHz (simplex)"
        );
        seed_cq_keeping_readback(&app, 7.074, panel).await;
        r.handle_key_event(key(' ')).await.unwrap();
        assert!(
            matches!(cmd_rx.try_recv(), Ok(TuiCommand::CallStation { ref callsign, frequency: 1234, dx_parity: Some(pancetta_core::slot::SlotParity::Even) }) if callsign == "N4BP")
        );
        assert_eq!(app.read().await.status_message(), "Calling N4BP...");
    }
}

#[tokio::test]
async fn space_calls_a_same_dial_ext_row_after_a_front_panel_qsy_to_another_band() {
    for panel in [ActivePanel::BandActivity, ActivePanel::DxHunter] {
        let (mut r, cmd_rx, app) = make_runner().await;
        band_keys_to_20m(&mut r).await;
        while cmd_rx.try_recv().is_ok() {}
        app.write().await.update_frequency_at(
            7_074_000,
            std::time::Instant::now() + std::time::Duration::from_secs(4),
        );
        assert_eq!(app.read().await.station_info.operating_frequency, 7.074);
        let state = app.read().await;
        assert_eq!(
            state.config.bands.bands[state.current_band_index].name,
            "40M"
        );
        drop(state);
        assert_eq!(
            app.read().await.status_message(),
            "Rig is on 7.074 MHz (40M)"
        );
        seed_cq_keeping_readback(&app, 7.074, panel).await;
        r.handle_key_event(key(' ')).await.unwrap();
        assert!(
            matches!(cmd_rx.try_recv(), Ok(TuiCommand::CallStation { ref callsign, frequency: 1234, .. }) if callsign == "N4BP")
        );
        assert_eq!(app.read().await.status_message(), "Calling N4BP...");
    }
}

#[tokio::test]
async fn a_lagging_readback_after_a_band_key_still_refuses() {
    let (mut r, cmd_rx, app) = make_runner().await;
    r.handle_message(TuiMessage::FrequencyUpdate {
        vfo: 0,
        frequency: 7_074_000,
    })
    .await
    .unwrap();
    r.handle_key_event(key('=')).await.unwrap();
    r.handle_key_event(key('=')).await.unwrap();
    while cmd_rx.try_recv().is_ok() {}
    r.handle_message(TuiMessage::FrequencyUpdate {
        vfo: 0,
        frequency: 7_074_000,
    })
    .await
    .unwrap();
    assert_eq!(app.read().await.station_info.operating_frequency, 14.074);
    seed_cq_keeping_readback(&app, 7.074, ActivePanel::BandActivity).await;
    r.handle_key_event(key(' ')).await.unwrap();
    assert!(cmd_rx.try_recv().is_err());
    assert_eq!(
        app.read().await.status_message(),
        "Can't work N4BP. WSJT-X heard it on 40m FT8. Pancetta is on 20m FT8 (rig reads 40m)."
    );
}

#[tokio::test]
async fn space_refuses_an_old_band_ext_row_while_a_frequency_modal_qsy_settles() {
    let (mut r, cmd_rx, app) = make_runner().await;
    band_keys_to_20m(&mut r).await;
    while cmd_rx.try_recv().is_ok() {}
    r.handle_message(TuiMessage::FrequencyUpdate {
        vfo: 0,
        frequency: 14_074_000,
    })
    .await
    .unwrap();
    submit_freq_modal(&mut r, "7.074").await;
    while cmd_rx.try_recv().is_ok() {}
    r.handle_message(TuiMessage::FrequencyUpdate {
        vfo: 0,
        frequency: 14_074_000,
    })
    .await
    .unwrap();
    assert_eq!(app.read().await.station_info.operating_frequency, 7.074);
    seed_cq_keeping_readback(&app, 14.074, ActivePanel::BandActivity).await;
    r.handle_key_event(key(' ')).await.unwrap();
    assert!(cmd_rx.try_recv().is_err());
    assert_eq!(
        app.read().await.status_message(),
        "Can't work N4BP. WSJT-X heard it on 20m FT8. Pancetta is on 40m FT8 (rig reads 20m)."
    );
}

#[tokio::test]
async fn space_uses_the_selected_ext_cq_after_qsy_with_a_retained_native_report() {
    let (mut r, cmd_rx, app) = make_runner().await;
    {
        let mut state = app.write().await;
        state.update_frequency(14_074_000);
        let text = format!("{} N4BP -08", state.station_info.call_sign);
        let mut report = ext_decode(&text, 14.074, "FT8", true);
        report.external = false;
        report.delta_freq = 1550.0;
        state.add_decoded_message(report).await.unwrap();
    }
    r.handle_message(TuiMessage::FrequencyUpdate {
        vfo: 0,
        frequency: 7_074_000,
    })
    .await
    .unwrap();
    seed_cq_keeping_readback(&app, 7.074, ActivePanel::BandActivity).await;
    // The retained directed report is pinned ahead of the CQ.
    app.write().await.band_activity_scroll = 1;
    r.handle_key_event(key(' ')).await.unwrap();
    let command = cmd_rx.try_recv().unwrap();
    assert!(
        matches!(command, TuiCommand::CallStation { ref callsign, frequency: 1234, dx_parity: Some(pancetta_core::slot::SlotParity::Even) } if callsign == "N4BP"),
        "expected to call the selected 40m CQ, got {command:?}"
    );
}

#[tokio::test]
async fn frequency_modal_rejects_a_dial_that_rounds_to_zero() {
    let (mut r, cmd_rx, app) = make_runner().await;
    r.handle_message(TuiMessage::FrequencyUpdate {
        vfo: 0,
        frequency: 14_074_000,
    })
    .await
    .unwrap();
    submit_freq_modal(&mut r, "0.0000001").await;

    let sent = cmd_rx.try_recv();
    assert!(sent.is_err(), "invalid dial sent {sent:?}");
    let state = app.read().await;
    assert!(state.freq_modal.visible);
    assert_eq!(state.station_info.operating_frequency, 14.074);
    assert!(state.status_message().starts_with("Invalid RX dial"));
}
