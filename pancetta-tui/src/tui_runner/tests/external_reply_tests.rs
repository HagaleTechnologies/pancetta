// PAN-435 (PAN-381). Space and the Callers reply refuse a decode heard only
// through a same-host WSJT-X when WSJT-X heard it on a band or mode other than
// pancetta's live dial and mode, with a status line and no command. A WSJT-X
// decode on pancetta's own band and mode is still acted on.
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
    let mut a = app.write().await;
    a.station_info.operating_frequency = 14.074;
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
    let mut a = app.write().await;
    a.station_info.operating_frequency = 14.074;
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

#[tokio::test]
async fn space_refuses_an_ext_row_from_another_band_or_mode() {
    for panel in [ActivePanel::BandActivity, ActivePanel::DxHunter] {
        for (dial_mhz, mode, heard) in [(7.074, "FT8", "40m FT8"), (14.080, "FT4", "20m FT4")] {
            let (mut r, cmd_rx, app) = make_runner().await;
            seed_cq(&app, dial_mhz, mode, panel).await;
            r.handle_key_event(key(' ')).await.unwrap();
            assert!(
                cmd_rx.try_recv().is_err(),
                "{panel:?}: Space on an EXT row heard on {heard} must send nothing"
            );
            assert_eq!(
                app.read().await.status_message(),
                format!("Can't work N4BP — WSJT-X heard it on {heard}, pancetta is on 20m FT8"),
                "{panel:?}"
            );
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
/// and the rig transmits on its own dial. A readback on pancetta's band does
/// not get in the way.
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
                "Can't work N4BP — WSJT-X heard it on {heard} FT8, pancetta is on 20m FT8 \
                 (rig reads 40m)"
            )
        );
    }

    let (mut r, cmd_rx, app) = make_runner().await;
    seed_cq(&app, 14.074, "FT8", ActivePanel::BandActivity).await;
    app.write().await.radio_frequency = Some(14.0745);
    r.handle_key_event(key(' ')).await.unwrap();
    assert!(matches!(
        cmd_rx.try_recv(),
        Ok(TuiCommand::CallStation { .. })
    ));
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
    for (dial_mhz, mode, heard) in [(7.074, "FT8", "40m FT8"), (14.080, "FT4", "20m FT4")] {
        // Space takes the reply branch for a decode directed at us.
        let (mut r, cmd_rx, app) = make_runner().await;
        seed_caller(&app, dial_mhz, mode).await;
        r.handle_key_event(key(' ')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err(), "Space reply to {heard}");
        let refusal =
            format!("Can't work N4BP — WSJT-X heard it on {heard}, pancetta is on 20m FT8");
        assert_eq!(app.read().await.status_message(), refusal);

        // Enter on the Callers panel replies to the selected caller.
        let (mut r, cmd_rx, app) = make_runner().await;
        seed_caller(&app, dial_mhz, mode).await;
        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(cmd_rx.try_recv().is_err(), "Callers Enter reply to {heard}");
        assert_eq!(app.read().await.status_message(), refusal);
    }
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
