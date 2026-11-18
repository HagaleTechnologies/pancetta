use super::key_tests::make_runner;
use super::*;

fn function_key(n: u8, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(KeyCode::F(n), mods)
}

#[tokio::test]
async fn drive_keys_send_steps_and_meter_renders_station_echo() {
    let (mut runner, commands, app) = make_runner().await;
    assert_eq!(app.read().await.tx_drive, None);
    runner
        .handle_key_event(function_key(7, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        commands.try_recv(),
        Ok(TuiCommand::AdjustTxDrive { delta_db: -1 })
    ));
    assert_eq!(
        app.read().await.tx_drive,
        None,
        "client must await station echo"
    );
    runner
        .handle_message(TuiMessage::TxDriveState {
            db: -1.0,
            ceiling_db: 0.0,
            mode: pancetta_core::tx_drive_mode::TxDriveMode::Manual,
            available: true,
        })
        .await
        .unwrap();
    assert_eq!(app.read().await.tx_drive, Some((-1.0, 0.0)));
    assert_eq!(
        app.read().await.tx_drive_mode,
        Some(pancetta_core::tx_drive_mode::TxDriveMode::Manual)
    );
    assert_eq!(
        crate::presentation::tx_meter_text_with_drive(
            crate::presentation::TxMeterRow::Idle,
            app.read().await.tx_drive
        ),
        " TX meter: drive −1 dB, shown while transmitting"
    );
    runner
        .handle_key_event(function_key(8, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        commands.try_recv(),
        Ok(TuiCommand::AdjustTxDrive { delta_db: 1 })
    ));
}

#[tokio::test]
async fn meter_identifies_the_ceiling_without_cat() {
    use crate::presentation::{tx_meter_text_with_drive, TxMeterRow};
    for row in [
        TxMeterRow::Idle,
        TxMeterRow::NeedsCat,
        TxMeterRow::NotReported,
    ] {
        assert!(tx_meter_text_with_drive(row, Some((0.0, 0.0))).contains("drive 0 dB (ceiling)"));
    }
    assert!(tx_meter_text_with_drive(TxMeterRow::NeedsCat, None).contains("drive pending"));
}

#[tokio::test]
async fn drive_keys_work_in_health_and_diagnostics_but_not_text_entry_or_modified() {
    let (mut runner, commands, app) = make_runner().await;
    for n in [7, 8, 9] {
        runner
            .handle_key_event(function_key(n, KeyModifiers::CONTROL))
            .await
            .unwrap();
        runner
            .handle_key_event(function_key(n, KeyModifiers::ALT))
            .await
            .unwrap();
    }
    assert!(commands.try_recv().is_err());
    app.write().await.show_health = true;
    runner
        .handle_key_event(function_key(7, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        commands.try_recv(),
        Ok(TuiCommand::AdjustTxDrive { delta_db: -1 })
    ));
    app.write().await.show_health = false;
    app.write().await.show_diagnostics = true;
    runner
        .handle_key_event(function_key(8, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        commands.try_recv(),
        Ok(TuiCommand::AdjustTxDrive { delta_db: 1 })
    ));
    runner
        .handle_key_event(function_key(9, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        commands.try_recv(),
        Ok(TuiCommand::ToggleTxDriveMode)
    ));
    app.write().await.show_diagnostics = false;
    app.write().await.enter_compose_mode();
    runner
        .handle_key_event(function_key(7, KeyModifiers::NONE))
        .await
        .unwrap();
    runner
        .handle_key_event(function_key(9, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(commands.try_recv().is_err());
}

#[tokio::test]
async fn drive_keys_emit_commands_and_render_manual_echo() {
    use pancetta_core::tx_drive_mode::TxDriveMode;
    let (mut runner, commands, app) = make_runner().await;
    runner
        .handle_key_event(function_key(8, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        commands.try_recv(),
        Ok(TuiCommand::AdjustTxDrive { delta_db: 1 })
    ));
    runner
        .handle_message(TuiMessage::TxDriveState {
            db: -2.0,
            ceiling_db: 0.0,
            mode: TxDriveMode::Manual,
            available: true,
        })
        .await
        .unwrap();
    let a = app.read().await;
    assert!(crate::presentation::tx_meter_text_with_drive_state(
        crate::presentation::TxMeterRow::Idle,
        a.tx_drive,
        a.tx_drive_mode,
        a.tx_drive_available
    )
    .contains("drive −2 dB (manual)"));
    drop(a);
    runner
        .handle_key_event(function_key(9, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        commands.try_recv(),
        Ok(TuiCommand::ToggleTxDriveMode)
    ));
}

#[tokio::test]
async fn meter_shows_auto_unavailable_without_alc() {
    use pancetta_core::tx_drive_mode::TxDriveMode;
    let (mut runner, _, app) = make_runner().await;
    runner
        .handle_message(TuiMessage::TxDriveState {
            db: -6.0,
            ceiling_db: 0.0,
            mode: TxDriveMode::Auto,
            available: false,
        })
        .await
        .unwrap();
    let a = app.read().await;
    let text = crate::presentation::tx_meter_text_with_drive_state(
        crate::presentation::TxMeterRow::Idle,
        a.tx_drive,
        a.tx_drive_mode,
        a.tx_drive_available,
    );
    assert!(text.contains("drive −6 dB (auto unavailable)"), "{text}");
    drop(a);
    runner
        .handle_message(TuiMessage::TxMeterCapability(
            pancetta_core::tx_meter::TxMeterCapability {
                forward_power: true,
                alc: false,
                alc_limit: None,
            },
        ))
        .await
        .unwrap();
    runner
        .handle_message(TuiMessage::TxDriveState {
            db: -6.0,
            ceiling_db: 0.0,
            mode: TxDriveMode::Auto,
            available: true,
        })
        .await
        .unwrap();
    let a = app.read().await;
    assert!(!a.tx_drive_available);
}

#[tokio::test]
async fn auto_overdrive_alert_renders_in_the_strip() {
    let (_, _, app) = make_runner().await;
    let mut a = app.write().await;
    a.add_error_message("ALC high: drive −3 dB".to_string());
    assert!(crate::readiness::readiness_items(&a)
        .iter()
        .any(|item| format!("{item:?}").contains("ALC high: drive −3 dB")));
    a.add_error_message("ALC high: lower the rig's USB/DATA input level".to_string());
    assert!(crate::readiness::readiness_items(&a)
        .iter()
        .any(|item| format!("{item:?}").contains("lower the rig's USB/DATA input level")));
}
