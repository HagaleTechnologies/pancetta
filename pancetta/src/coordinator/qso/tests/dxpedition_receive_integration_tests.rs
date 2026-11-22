use super::*;
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Modulator, Protocol, WINDOW_SAMPLES};
use pancetta_qso::{QsoEvent, QsoManager, QsoManagerConfig, QsoState};

// Independently packed by gen_dxpedition_type01.py. The second payload
// swaps the two Hounds so W9XYZ receives RR73 after its report.
const REPORT: [u8; 10] = [0x09, 0xBD, 0xE3, 0x50, 0xC2, 0x93, 0xB8, 0x32, 0x56, 0x40];
const CLOSE: [u8; 10] = [0x0C, 0x29, 0x3B, 0x80, 0x9B, 0xDE, 0x35, 0x32, 0x56, 0x40];

fn receive(
    decoder: &mut Ft8Decoder,
    payload: &[u8; 10],
    hz: f64,
) -> Vec<pancetta_ft8::DecodedMessage> {
    let mut symbols = [0; 79];
    // SAFETY: payload has 10 live bytes and the exclusive output has 79 bytes; C retains neither pointer.
    unsafe {
        pancetta_ft8::ft8_lib_ffi::ft8_encode(payload.as_ptr(), symbols.as_mut_ptr());
    }
    let mut audio = Ft8Modulator::new(12_000, hz, 0.5)
        .unwrap()
        .modulate_symbols(&symbols, 0.0)
        .unwrap();
    audio.resize(WINDOW_SAMPLES, 0.0);
    let native = decoder.decode_window(&audio).unwrap();
    let reference = decoder.decode_window_ft8lib_with_context(&audio, Protocol::Ft8);
    let mut merged = pancetta_ft8::merge::merge_ft8lib_first(reference, native).messages;
    for msg in &mut merged {
        msg.captured_dial_hz = Some(14_074_000);
        msg.slot_parity = Some(pancetta_core::slot::SlotParity::Even);
    }
    merged
}

fn events(rx: &mut tokio::sync::broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

async fn deliver(
    manager: &QsoManager,
    frame: &pancetta_ft8::DecodedMessage,
    live_dial: u64,
    call: &str,
) -> usize {
    // The same pre-expansion band guard and plural adapter as the bus arm.
    if super::super::is_stale_band_decode(frame.captured_dial_hz, live_dial) {
        return 0;
    }
    let contacts = ft8_message_to_qso_types(&frame.message, &frame.text, call);
    let count = contacts.len();
    for contact in contacts {
        manager
            .process_message_with_parity(
                contact,
                frame.text.clone(),
                frame.frequency_offset,
                Some(frame.snr_db),
                frame.slot_parity,
            )
            .await
            .unwrap();
    }
    count
}

#[tokio::test]
async fn dxpedition_unknown_and_collided_fox_audio_yields_no_actions() {
    let manager = QsoManager::new(QsoManagerConfig {
        our_callsign: "W9XYZ".into(),
        ..Default::default()
    });
    let id = manager
        .engage_hound("KH1/KH7Z", 600.0, None, None)
        .await
        .unwrap();
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
    decoder.set_hound_foxes(&["W1ABC".into()]);
    for collided in [false, true] {
        if collided {
            // Independent fixture generator: both hashes are 201.
            decoder.seed_hash_callsign("W2X93");
            decoder.set_hound_foxes(&["KH1/KH7Z".into()]);
        }
        let frames = receive(&mut decoder, &REPORT, 600.0);
        let frame = frames
            .iter()
            .find(|m| m.message.dxpedition.is_some())
            .unwrap();
        assert!(frame
            .message
            .dxpedition
            .as_ref()
            .unwrap()
            .fox_call
            .is_none());
        assert_eq!(deliver(&manager, frame, 14_074_000, "W9XYZ").await, 0);
        assert!(!manager.get_qso(id).await.unwrap().metadata.hound_qsyed);
        if collided {
            let mut rebuilt = Ft8Decoder::new(Ft8Config::default()).unwrap();
            for call in decoder.learned_callsigns() {
                rebuilt.seed_hash_callsign(&call);
            }
            rebuilt.set_hound_foxes(&["KH1/KH7Z".into()]);
            let frames = receive(&mut rebuilt, &REPORT, 600.0);
            let frame = frames
                .iter()
                .find(|m| m.message.dxpedition.is_some())
                .unwrap();
            assert_eq!(deliver(&manager, frame, 14_074_000, "W9XYZ").await, 0);
        }
    }
}

#[tokio::test]
async fn dxpedition_independent_audio_reaches_hound_qso_and_completes_once_without_73() {
    for (call, report_payload, close_payload) in
        [("W9XYZ", REPORT, CLOSE), ("K1ABC", CLOSE, REPORT)]
    {
        let manager = QsoManager::new(QsoManagerConfig {
            our_callsign: call.into(),
            our_grid: Some("EN50".into()),
            ..Default::default()
        });
        let mut rx = manager.subscribe();
        let id = manager
            .engage_hound(
                "KH1/KH7Z",
                600.0,
                None,
                Some(pancetta_core::slot::SlotParity::Even),
            )
            .await
            .unwrap();
        let opening = events(&mut rx);
        assert!(opening.iter().any(|e| matches!(e,
        QsoEvent::MessageToSend { frequency, .. } if (1000.0..=2700.0).contains(frequency))));
        let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
        decoder.set_hound_foxes(&manager.active_hound_foxes().await);
        let frames = receive(&mut decoder, &report_payload, 600.0);
        let fox_frames: Vec<_> = frames
            .iter()
            .filter(|m| m.message.dxpedition.is_some())
            .collect();
        assert_eq!(fox_frames.len(), 1, "one physical relay/display row");
        let frame = fox_frames[0];
        assert_eq!(deliver(&manager, frame, 7_074_000, call).await, 0);
        assert!(events(&mut rx).is_empty());
        assert!(!manager.get_qso(id).await.unwrap().metadata.hound_qsyed);
        assert_eq!(deliver(&manager, frame, 14_074_000, call).await, 2);
        let reports = events(&mut rx);
        assert!(reports.iter().any(|e| matches!(e,
        QsoEvent::MessageToSend { frequency, message: pancetta_qso::MessageType::ReportAck { .. }, .. }
        if *frequency == frame.frequency_offset)));
        assert_eq!(
            manager.get_qso(id).await.unwrap().metadata.frequency,
            frame.frequency_offset
        );
        assert_eq!(
            manager.get_active_qsos().await.len(),
            1,
            "no sibling QSO for the other Hound"
        );

        // A later stream >100 Hz away must survive the production receiver.
        let moved = receive(&mut decoder, &report_payload, 780.0);
        let moved = moved
            .iter()
            .find(|m| m.message.dxpedition.is_some())
            .unwrap();
        deliver(&manager, moved, 14_074_000, call).await;
        assert_eq!(
            manager.get_qso(id).await.unwrap().metadata.frequency,
            moved.frequency_offset
        );
        events(&mut rx);
        let closed = receive(&mut decoder, &close_payload, 780.0);
        let closed = closed
            .iter()
            .find(|m| m.message.dxpedition.is_some())
            .unwrap();
        deliver(&manager, closed, 14_074_000, call).await;
        deliver(&manager, closed, 14_074_000, call).await;
        let completed = events(&mut rx);
        assert_eq!(
            completed
                .iter()
                .filter(|e| matches!(e, QsoEvent::QsoCompleted { .. }))
                .count(),
            1
        );
        assert!(!completed
            .iter()
            .any(|e| matches!(e, QsoEvent::MessageToSend { .. })));
        assert!(matches!(
            manager.get_qso(id).await.unwrap().state,
            QsoState::Completed { .. }
        ));
    }
}
