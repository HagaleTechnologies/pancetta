// PAN-122. Formerly the inline module `pan122_tx_log_worker_tests`.
use super::super::tx_log::{
    tx_log_path, RotationPolicy, TxFrameLog, TxFrameRecord, TxLogAppend, WriterGate,
    TX_LOG_ACK_WAIT,
};
use super::super::ApplicationCoordinator;
use super::worker_gate_tests::{
    await_completion, request, worker_harness, worker_harness_with, Harness,
};
use crate::message_bus::{
    ComponentId, ComponentMessage, MessageType, RigControlMessage, TransmitRequestItem, TxOrigin,
};
use pancetta_config::Config;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TEXT: &str = "CQ N0CALL FN20";

fn tx_log_lines(h: &Harness) -> Vec<TxFrameRecord> {
    let path = tx_log_path(h.coordinator.pancetta_home_override.as_deref());
    match std::fs::read_to_string(&path) {
        Ok(content) => content
            .lines()
            .map(|line| serde_json::from_str(line).expect("one record per line"))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("read {}: {e}", path.display()),
    }
}

async fn await_tx_log_lines(h: &Harness, n: usize, within: Duration) -> Vec<TxFrameRecord> {
    let deadline = Instant::now() + within;
    loop {
        let lines = tx_log_lines(h);
        if lines.len() >= n || Instant::now() >= deadline {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Fill `dst`'s channel until the bus refuses another message.
/// `pub(crate)`: shared with the PAN-182 audio-delivery module, the way
/// the PAN-115 harness is shared with this one.
pub(crate) async fn fill_channel(h: &Harness, dst: ComponentId) -> usize {
    let mut sent = 0;
    while h
        .coordinator
        .message_bus
        .send_message_checked(ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            dst,
            MessageType::TxStatus { active: false },
            Instant::now(),
        ))
        .await
        .unwrap()
    {
        sent += 1;
    }
    sent
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_frame_is_durably_logged_even_when_the_tui_channel_is_full() {
    let h = worker_harness().await;
    assert!(fill_channel(&h, ComponentId::Tui).await > 0);
    let (_, audio_rx) = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap();

    request(&h, TEXT).await;
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut audio_enqueued = false;
    while !audio_enqueued && Instant::now() < deadline {
        while let Ok(msg) = audio_rx.try_recv() {
            audio_enqueued |= matches!(msg.message_type, MessageType::AudioOutput { .. });
        }
        while h.hamlib_rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(audio_enqueued, "the frame's audio must reach the output");

    // File visibility precedes the best-effort TUI relay attempt. Keep
    // TUI full until this frame completes, which happens after the relay.
    // Leave Autonomous unread above so an early completion is retained.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut completed = None;
    while completed.is_none() && Instant::now() < deadline {
        while h.hamlib_rx.try_recv().is_ok() {}
        while let Ok(msg) = h.autonomous_rx.try_recv() {
            if let MessageType::TransmitComplete {
                success,
                message_text,
                qso_id: None,
                ..
            } = msg.message_type
            {
                if message_text == TEXT {
                    completed = Some(success);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        completed,
        Some(true),
        "the frame must complete successfully before draining TUI"
    );

    let lines = await_tx_log_lines(&h, 1, Duration::from_secs(5)).await;
    assert_eq!(lines.len(), 1, "exactly one record: {lines:?}");
    let r = &lines[0];
    assert_eq!(r.v, 1);
    assert_eq!(r.text, TEXT);
    assert_eq!(r.mode, "FT8");
    assert_eq!(r.offset_hz, 1500.0);
    assert_eq!(r.origin, TxOrigin::Local);
    assert_eq!(r.qso_id, None);
    assert_eq!(r.dial_hz, None, "dial unknown in the harness");

    // Completion proves the relay already encountered the full route;
    // draining now cannot admit a late TxFrameLogged event.
    let tui_frame_events = h
        .tui_rx
        .try_iter()
        .filter(|m| matches!(m.message_type, MessageType::TxFrameLogged { .. }))
        .count();
    assert_eq!(
        tui_frame_events, 0,
        "the TUI route was full, so the record cannot have come from it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_carries_the_effective_split_aware_dial() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    h.coordinator
        .split_tx_frequency_hz
        .store(14_080_000, Ordering::Relaxed);

    request(&h, TEXT).await;
    let lines = await_tx_log_lines(&h, 1, Duration::from_secs(45)).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].dial_hz, Some(14_080_000), "split dial while split");
    // Let the first frame finish before the next request, so the second
    // is keyed on its own rather than superseding the first mid-playback.
    let (completed, _, _) = await_completion(&h, TEXT, Duration::from_secs(30)).await;
    assert_eq!(completed, Some(true));

    h.coordinator
        .split_tx_frequency_hz
        .store(0, Ordering::Relaxed);
    request(&h, "CQ N0CALL EN27").await;
    let lines = await_tx_log_lines(&h, 2, Duration::from_secs(50)).await;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[1].text, "CQ N0CALL EN27");
    assert_eq!(lines[1].dial_hz, Some(14_074_000), "RX dial when simplex");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundle_writes_one_record_per_item_with_one_shared_timestamp() {
    let h = worker_harness().await;
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::MultiTransmitRequest {
                items: vec![
                    TransmitRequestItem {
                        attempt_id: None,
                        message_text: "CQ N0CALL FN20".to_string(),
                        frequency_offset: 1000.0,
                        qso_id: None,
                        origin: TxOrigin::Local,
                        remote_client_key_id: None,
                    },
                    TransmitRequestItem {
                        attempt_id: None,
                        message_text: "CQ N0CALL EN27".to_string(),
                        frequency_offset: 1600.0,
                        qso_id: None,
                        origin: TxOrigin::Local,
                        remote_client_key_id: None,
                    },
                ],
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();

    let lines = await_tx_log_lines(&h, 2, Duration::from_secs(45)).await;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0].text, "CQ N0CALL FN20");
    assert_eq!(lines[0].offset_hz, 1000.0);
    assert_eq!(lines[1].text, "CQ N0CALL EN27");
    assert_eq!(lines[1].offset_hz, 1600.0);
    assert_eq!(lines[0].utc, lines[1].utc, "one bundle, one key instant");
    assert_eq!(
        [lines[0].origin, lines[1].origin],
        [TxOrigin::Local, TxOrigin::Local],
        "each record carries its own item's origin"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_timestamp_matches_tx_frame_logged() {
    let h = worker_harness().await;
    request(&h, TEXT).await;

    let deadline = Instant::now() + Duration::from_secs(45);
    let mut event = None;
    while event.is_none() && Instant::now() < deadline {
        while let Ok(msg) = h.tui_rx.try_recv() {
            if let MessageType::TxFrameLogged {
                text,
                freq_hz,
                qso_id,
                timestamp,
            } = msg.message_type
            {
                event = Some((text, freq_hz, qso_id, timestamp));
            }
        }
        while h.hamlib_rx.try_recv().is_ok() {}
        while h.autonomous_rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (text, freq_hz, qso_id, timestamp) = event.expect("TxFrameLogged must arrive");

    let lines = await_tx_log_lines(&h, 1, Duration::from_secs(5)).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].utc, timestamp);
    assert_eq!(lines[0].text, text);
    assert_eq!(lines[0].offset_hz, freq_hz);
    assert_eq!(lines[0].qso_id, qso_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frame_whose_audio_is_not_enqueued_leaves_no_record() {
    let h = worker_harness().await;
    assert!(fill_channel(&h, ComponentId::Audio).await > 0);

    request(&h, TEXT).await;
    let (completed, _, ptt_on) = await_completion(&h, TEXT, Duration::from_secs(45)).await;
    assert!(completed.is_some(), "the frame must complete");
    assert!(ptt_on, "PTT keyed, so only the audio enqueue was dropped");
    assert!(
        tx_log_lines(&h).is_empty(),
        "dropped audio never aired, so it must not be recorded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frame_refused_before_ptt_leaves_no_record() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(15_000_000, Ordering::Relaxed);

    request(&h, TEXT).await;
    let (completed, _, ptt_on) = await_completion(&h, TEXT, Duration::from_secs(5)).await;
    assert_eq!(completed, Some(false));
    assert!(!ptt_on);
    assert!(tx_log_lines(&h).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_tx_log_never_holds_ptt_past_playback() {
    let gate = WriterGate::closed();
    let writer_gate = gate.clone();
    let h = worker_harness_with(Config::default(), move |c| {
        let path = tx_log_path(c.pancetta_home_override.as_deref());
        c.tx_frame_log = Some(Arc::new(TxFrameLog::open_gated(
            path,
            RotationPolicy::default(),
            writer_gate,
        )));
    })
    .await;
    let (_, audio_rx) = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap();

    request(&h, TEXT).await;
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut ptt_on = false;
    let mut ptt_off_after_on = false;
    let mut ptt_off_at = None;
    let mut audio = None;
    let mut completed = None;
    while completed.is_none() && Instant::now() < deadline {
        while let Ok(msg) = audio_rx.try_recv() {
            if let MessageType::AudioOutput {
                samples,
                sample_rate,
                ..
            } = msg.message_type
            {
                let len = Duration::from_secs_f64(samples.len() as f64 / sample_rate as f64);
                audio.get_or_insert((Instant::now(), len));
            }
        }
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if let MessageType::RigControl(RigControlMessage::SetPtt { state }) = msg.message_type {
                if state {
                    ptt_on = true;
                } else if ptt_on {
                    ptt_off_after_on = true;
                    ptt_off_at.get_or_insert(Instant::now());
                }
            }
        }
        while let Ok(msg) = h.autonomous_rx.try_recv() {
            if let MessageType::TransmitComplete {
                success,
                message_text,
                ..
            } = msg.message_type
            {
                if message_text == TEXT {
                    completed = Some(success);
                }
            }
        }
        while h.tui_rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let off_deadline = Instant::now() + Duration::from_secs(2);
    while !ptt_off_after_on && Instant::now() < off_deadline {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if let MessageType::RigControl(RigControlMessage::SetPtt { state: false }) =
                msg.message_type
            {
                if ptt_on {
                    ptt_off_after_on = true;
                    ptt_off_at.get_or_insert(Instant::now());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        completed,
        Some(true),
        "the frame completes while the writer is blocked"
    );
    assert!(ptt_on, "PTT keyed");
    assert!(ptt_off_after_on, "PTT released while the writer is blocked");
    // PR #468 review: the full TX_LOG_ACK_WAIT spent waiting on the blocked
    // writer must not be added to key-down after the audio ends.
    let (audio_at, audio_len) = audio.expect("audio enqueued");
    let ptt_off_at = ptt_off_at.expect("PTT-off observed in the playback loop");
    let overhang = ptt_off_at.saturating_duration_since(audio_at + audio_len);
    assert!(
        overhang < TX_LOG_ACK_WAIT - Duration::from_millis(50),
        "PTT stayed keyed {overhang:?} past the audio (log wait leaked into playback)"
    );
    assert!(
        tx_log_lines(&h).is_empty(),
        "the blocked writer wrote nothing"
    );

    gate.open();
    let lines = await_tx_log_lines(&h, 1, Duration::from_secs(5)).await;
    assert_eq!(
        lines.len(),
        1,
        "the queued record lands once the disk responds"
    );
    assert_eq!(lines[0].text, TEXT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tx_frame_log_is_created_once_and_reused_across_restarts() {
    let mut h = worker_harness().await;
    let first = h
        .coordinator
        .tx_frame_log
        .clone()
        .expect("created by start_transmitter_component");
    let home = h.coordinator.pancetta_home_override.clone().unwrap();
    assert!(first.path().expect("enabled").starts_with(&home));
    let again = h.coordinator.ensure_tx_frame_log();
    assert!(Arc::ptr_eq(&first, &again), "the restart reuses the writer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_mode_tx_log_is_disabled_and_creates_no_file() {
    let mut coordinator = ApplicationCoordinator::new(
        Config::default(),
        None,
        true,                                                // no_audio
        true,                                                // headless
        None,                                                // no WAV
        Some(std::path::PathBuf::from("/some/capture/dir")), // --replay
        None,                                                // no test-tx
        1500.0,
        Arc::new(AtomicBool::new(false)),
        Vec::new(),
        std::env::temp_dir().join(format!(
            "pancetta-pan122-config-{}.toml",
            uuid::Uuid::new_v4()
        )),
        true,
        None, // PAN-270: no session-home override
    )
    .await
    .expect("coordinator creation should succeed");
    let home = tempfile::tempdir().unwrap();
    coordinator.pancetta_home_override = Some(home.path().to_path_buf());

    let log = coordinator.ensure_tx_frame_log();
    assert_eq!(log.path(), None);
    let record = TxFrameRecord::new(
        chrono::Utc::now(),
        pancetta_ft8::Protocol::Ft8,
        0,
        1500.0,
        TEXT.to_string(),
        TxOrigin::Local,
        None,
    );
    assert_eq!(
        log.append(vec![record], Duration::from_secs(1)).await,
        TxLogAppend::Disabled
    );
    assert!(!tx_log_path(Some(home.path())).exists());
}
