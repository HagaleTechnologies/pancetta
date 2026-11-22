// PAN-435, PAN-443 (PAN-381). Space and the Callers reply refuse a decode
// heard only through a same-host WSJT-X unless WSJT-X heard it within 10 Hz of
// pancetta's dial (the rig readback when known, else the operating frequency)
// and in pancetta's mode, with a status line and no command. A WSJT-X decode
// on pancetta's own dial and mode is acted on, including on a band
// `pancetta_core::Band` cannot name.
use super::key_tests::{key, make_runner};
use super::*;
use crate::app::{ActivePanel, DecodedMessageView};

/// A decode a same-host WSJT-X sent, as the relay builds it: dial in MHz and
/// the mode WSJT-X named.
fn ext_decode(text: &str, dial_mhz: f64, mode: &str, directed: bool) -> DecodedMessageView {
    DecodedMessageView {
        timestamp: chrono::Utc::now(),
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
