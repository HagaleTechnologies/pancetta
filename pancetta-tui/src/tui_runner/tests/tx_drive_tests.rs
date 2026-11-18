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
        })
        .await
        .unwrap();
    assert_eq!(app.read().await.tx_drive, Some((-1.0, 0.0)));
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
    for n in [7, 8] {
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
    app.write().await.show_diagnostics = false;
    app.write().await.enter_compose_mode();
    runner
        .handle_key_event(function_key(7, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(commands.try_recv().is_err());
}
