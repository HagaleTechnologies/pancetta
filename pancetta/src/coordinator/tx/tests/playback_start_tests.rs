// PAN-71. Formerly the inline module `pan71_playback_start_tests`.
use super::super::tx_log::{tx_log_path, RotationPolicy, WriterGate};
use super::worker_gate_tests::{build_worker_harness, request, Harness};
use super::*;
use crate::coordinator::ApplicationCoordinator;
use crate::message_bus::{RigControlMessage, TransmitRequestItem, TxOrigin};
use pancetta_config::Config;
use std::sync::{Arc, Mutex};

// Tokio's elapsed time and UTC share one origin on each paused runtime.

fn virtual_clock() -> pancetta_core::time::UtcClock {
    // All harnesses in a test are constructed before virtual time advances.
    let origin = tokio::time::Instant::now();
    Arc::new(move || {
        pancetta_core::time::slot_grid_origin()
            + SignedDuration::try_from(origin.elapsed()).unwrap()
    })
}

async fn worker_harness() -> Harness {
    worker_harness_with(Config::default(), |_| {}).await
}

async fn worker_harness_with(
    config: Config,
    prepare: impl FnOnce(&mut ApplicationCoordinator),
) -> Harness {
    build_worker_harness(
        config,
        prepare,
        Arc::new(pancetta_agent::arm::ArmTime::now),
        virtual_clock(),
        true,
    )
    .await
}

const TEXT: &str = "CQ N0CALL FN20";
const REPLACEMENT: &str = "CQ N0CALL EM12";

#[derive(Clone, Copy)]
enum Behaviour {
    StartAfter(Duration),
    NotStarted,
    Drop,
    Hold,
}

#[derive(Clone, Debug)]
struct Seen {
    received_at: UtcTimestamp,
    flush_first: bool,
    started_at: Option<UtcTimestamp>,
}

struct FakeAudio {
    seen: Arc<Mutex<Vec<Seen>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeAudio {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_fake_audio_thread(h: &Harness, behaviour: Behaviour) -> FakeAudio {
    let (_, rx) = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let utc_clock = h.utc_clock.clone();
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            while let Ok(msg) = rx.try_recv() {
                if let MessageType::AudioOutput {
                    playback_started,
                    flush_first,
                    ..
                } = msg.message_type
                {
                    let ack = playback_started.expect("logged frame must carry an ack");
                    let received_at = utc_clock();
                    let started_at = if let Behaviour::StartAfter(d) = behaviour {
                        Some(received_at + SignedDuration::try_from(d).unwrap())
                    } else {
                        None
                    };
                    captured.lock().unwrap().push(Seen {
                        received_at,
                        flush_first,
                        started_at,
                    });
                    match behaviour {
                        Behaviour::StartAfter(d) => {
                            sleep(d).await;
                            ack.started(started_at.unwrap());
                        }
                        Behaviour::NotStarted => ack.not_started(),
                        Behaviour::Drop => drop(ack),
                        Behaviour::Hold => held.push(ack),
                    }
                }
            }
            sleep(Duration::from_millis(1)).await;
        }
    });
    FakeAudio { seen, task }
}

async fn await_event(h: &Harness, wanted: &str) -> UtcTimestamp {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        while let Ok(msg) = h.tui_rx.try_recv() {
            if let MessageType::TxFrameLogged {
                text, timestamp, ..
            } = msg.message_type
            {
                if text == wanted {
                    return timestamp;
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no TxFrameLogged for {wanted}"
        );
        sleep(Duration::from_millis(1)).await;
    }
}

fn tx_log_lines(h: &Harness) -> Vec<TxFrameRecord> {
    let path = tx_log_path(h.coordinator.pancetta_home_override.as_deref());
    match std::fs::read_to_string(path) {
        Ok(contents) => contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("reading TX log: {e}"),
    }
}

async fn await_tx_log_lines(h: &Harness, n: usize) -> Vec<TxFrameRecord> {
    h.coordinator
        .tx_frame_log
        .as_ref()
        .unwrap()
        .drain_for_test();
    let lines = tx_log_lines(h);
    assert!(
        lines.len() >= n,
        "writer drained without the expected records"
    );
    lines
}

#[tokio::test(start_paused = true)]
async fn worker_stamps_come_from_the_injected_clock() {
    let mut config = Config::default();
    config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;
    let pinned = pancetta_core::time::slot_grid_origin();
    let h = build_worker_harness(
        config,
        |_| {},
        Arc::new(pancetta_agent::arm::ArmTime::now),
        Arc::new(move || pinned),
        true,
    )
    .await;
    let _audio = spawn_fake_audio_thread(&h, Behaviour::NotStarted).await;
    request(&h, TEXT).await;
    assert_eq!(await_event(&h, TEXT).await, pinned);
    assert_eq!(await_record(&h).await.utc, pinned);
}

#[tokio::test(start_paused = true)]
async fn first_pass_frame_is_stamped_at_the_reported_playback_start() {
    let h = worker_harness().await;
    let audio = spawn_fake_audio_thread(&h, Behaviour::StartAfter(Duration::from_millis(37))).await;
    request(&h, TEXT).await;
    let at = await_event(&h, TEXT).await;
    let seen = audio.seen.lock().unwrap()[0].clone();
    assert!(!seen.flush_first);
    assert_eq!(Some(at), seen.started_at);
    assert!(at > seen.received_at);
    let lines = await_tx_log_lines(&h, 1).await;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].utc, at);
}

#[tokio::test(start_paused = true)]
async fn rekeyed_frame_is_stamped_at_the_replacement_audios_playback_start() {
    // Auto picks the NEXT slot on each request. Pin parity so the
    // replacement can re-key this slot within the normal 3000 ms cap.
    let mut config = Config::default();
    config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;
    let h = worker_harness_with(config, |_| {}).await;
    let audio = spawn_fake_audio_thread(&h, Behaviour::StartAfter(Duration::from_millis(37))).await;
    request(&h, TEXT).await;
    let first = await_event(&h, TEXT).await;
    request(&h, REPLACEMENT).await;
    let second = await_event(&h, REPLACEMENT).await;
    let seen = audio.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen[1].flush_first, "exercise an actual mid-frame re-key");
    assert_eq!(Some(second), seen[1].started_at);
    assert!(second > seen[1].received_at);
    assert!(second > first);
    let lines = await_tx_log_lines(&h, 2).await;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1].text, REPLACEMENT);
    assert_eq!(lines[1].utc, second);
}

/// One frame through `h`: `(stamp, when the event was observed, what audio saw)`.
async fn fallback_event(h: &Harness, behaviour: Behaviour) -> (UtcTimestamp, UtcTimestamp, Seen) {
    let audio = spawn_fake_audio_thread(h, behaviour).await;
    request(h, TEXT).await;
    let at = await_event(h, TEXT).await;
    let observed = (h.utc_clock)();
    let seen = audio.seen.lock().unwrap()[0].clone();
    // The enqueue instant, not the timeout instant a bound later.
    assert!(at <= seen.received_at + SignedDuration::try_from(TX_PLAYBACK_START_WAIT / 2).unwrap());
    (at, observed, seen)
}

/// `h`'s one TX-log record. `TX_LOG_ACK_WAIT` caps how long the worker
/// waits for the append, not the write, so the record can land after
/// `TxFrameLogged` (PAN-248).
async fn await_record(h: &Harness) -> TxFrameRecord {
    let lines = await_tx_log_lines(h, 1).await;
    lines.into_iter().next().expect("the frame's TX-log record")
}

async fn assert_fallback(behaviour: Behaviour) {
    let h = worker_harness().await;
    if matches!(behaviour, Behaviour::Hold) {
        let (at, observed, _) = fallback_event(&h, behaviour).await;
        assert!(observed - at >= SignedDuration::from_millis(98));
        assert_eq!(await_record(&h).await.utc, at);
        return;
    }
    // PAN-248: the durable append (up to TX_LOG_ACK_WAIT) runs between the
    // wait and TxFrameLogged, so time the settled ack on a harness without it.
    let timed = worker_harness_with(Config::default(), |c| {
        c.tx_frame_log = Some(Arc::new(TxFrameLog::disabled()))
    })
    .await;
    let ((at, _, _), (_, observed, seen)) = tokio::join!(
        fallback_event(&h, behaviour),
        fallback_event(&timed, behaviour)
    );
    assert!(
        observed - seen.received_at < SignedDuration::try_from(TX_PLAYBACK_START_WAIT).unwrap()
    );
    assert_eq!(await_record(&h).await.utc, at);
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_playback_start_falls_back_to_the_enqueue_instant_after_the_bound() {
    assert_fallback(Behaviour::Hold).await;
}
#[tokio::test(start_paused = true)]
async fn not_started_report_falls_back_to_the_enqueue_instant_without_waiting() {
    assert_fallback(Behaviour::NotStarted).await;
}
#[tokio::test(start_paused = true)]
async fn dropped_ack_falls_back_to_the_enqueue_instant_without_waiting() {
    assert_fallback(Behaviour::Drop).await;
}

#[tokio::test(start_paused = true)]
async fn bundle_keys_without_waiting_on_wall_time() {
    use pancetta_core::slot::SlotParity;

    let mut config = Config::default();
    config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;
    // Headroom for a loaded runner: the requeued replacement must still
    // be accepted into the slot the first bundle keyed at its boundary.
    config.station.tx_late_max_ms = 10_000;
    let h = worker_harness_with(config, |_| {}).await;
    let (_, audio_rx) = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap();
    {
        let mut live = h.coordinator.active_tx_qsos.write().unwrap();
        for id in ["pan294-a", "pan294-b"] {
            live.insert(super::super::active_tx_qso_key(id));
        }
    }
    let submit = |texts: [&str; 2]| {
        ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::MultiTransmitRequest {
                // Matching live identities and changed text force a supersede.
                items: texts
                    .into_iter()
                    .enumerate()
                    .map(|(i, text)| TransmitRequestItem {
                        attempt_id: None,
                        message_text: text.to_owned(),
                        frequency_offset: 1000.0 + i as f64 * 600.0,
                        qso_id: Some(["pan294-a", "pan294-b"][i].to_owned()),
                        origin: TxOrigin::Local,
                        remote_client_key_id: None,
                    })
                    .collect(),
                tx_parity: Some(SlotParity::Even),
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            tokio::time::Instant::now().into_std(),
        )
    };
    let initial = [TEXT, "CQ K1ABC FN31"];
    let replacement = [REPLACEMENT, "CQ K1ABC EM12"];
    let mut waveforms = Vec::new();
    let mut buffered = Vec::new();
    let mut flushes = Vec::new();
    let mut starts = Vec::new();
    let mut ptt = Vec::new();
    // A requeued bundle is dequeued afresh and only takes the current slot
    // inside `multi_tx_acceptance_cap_ms`. Submit from an odd slot so the
    // even bundle keys at its boundary, not late in a slot already running.
    tokio::time::advance(Duration::from_secs(15)).await;
    h.coordinator
        .message_bus
        .send_message(submit(initial))
        .await
        .unwrap();
    for texts in [initial, replacement] {
        // Simulate an audio callback that starts playback but retains all
        // queued samples. Only a flush can remove the predecessor.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        loop {
            if let Ok(msg) = audio_rx.try_recv() {
                if let MessageType::AudioOutput {
                    samples,
                    flush_first,
                    playback_started,
                    ..
                } = msg.message_type
                {
                    assert!(!samples.is_empty());
                    if flush_first {
                        buffered.clear();
                    }
                    buffered.extend_from_slice(&samples);
                    waveforms.push(samples);
                    flushes.push(flush_first);
                    playback_started
                        .expect("bundle playback ack")
                        .started((h.utc_clock)());
                    break;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no bundle audio for {texts:?}"
            );
            sleep(Duration::from_millis(1)).await;
        }
        // Both log events prove both streams reached playback. Awaiting
        // them also ensures replacement arrives after the first ack.
        let first = await_event(&h, texts[0]).await;
        assert_eq!(first, await_event(&h, texts[1]).await);
        starts.push(first);
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if let MessageType::RigControl(RigControlMessage::SetPtt { state }) = msg.message_type {
                ptt.push(state);
            }
        }
        if waveforms.len() == 1 {
            assert_eq!(buffered, waveforms[0]);
            h.coordinator
                .message_bus
                .send_message(submit(replacement))
                .await
                .unwrap();
        }
    }
    assert_eq!(waveforms.len(), 2);
    assert_ne!(waveforms[0], waveforms[1], "supersede changes the waveform");
    assert_eq!(
        starts[0].as_second().div_euclid(15),
        starts[1].as_second().div_euclid(15),
        "replacement must requeue into the same FT8 slot"
    );
    assert_eq!(
        ptt,
        [true, false, true],
        "unkey before the replacement keys"
    );
    assert!(flushes[1], "replacement bundle must flush stale audio");
    assert_eq!(buffered, waveforms[1], "only replacement samples remain");
}

#[tokio::test(start_paused = true)]
async fn bundle_items_share_the_reported_playback_start() {
    let h = worker_harness().await;
    let audio = spawn_fake_audio_thread(&h, Behaviour::StartAfter(Duration::from_millis(37))).await;
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::MultiTransmitRequest {
                items: [TEXT, REPLACEMENT]
                    .into_iter()
                    .enumerate()
                    .map(|(i, text)| TransmitRequestItem {
                        attempt_id: None,
                        message_text: text.to_owned(),
                        frequency_offset: 1000.0 + i as f64 * 600.0,
                        qso_id: None,
                        origin: TxOrigin::Local,
                        remote_client_key_id: None,
                    })
                    .collect(),
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            tokio::time::Instant::now().into_std(),
        ))
        .await
        .unwrap();
    let first = await_event(&h, TEXT).await;
    let second = await_event(&h, REPLACEMENT).await;
    let seen = audio.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(Some(first), seen[0].started_at);
    assert_eq!(first, second);
    let lines = await_tx_log_lines(&h, 2).await;
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().all(|r| r.utc == first));
}

#[tokio::test(start_paused = true)]
async fn frame_log_instant_started_uses_reported_time() {
    let (ack, mut waiter) = PlaybackStartAck::new();
    let fallback = pancetta_core::time::slot_grid_origin();
    let at = fallback + SignedDuration::from_millis(37);
    ack.started(at);
    assert_eq!(frame_log_instant(&mut waiter, fallback, TEXT).await, at);
}
#[tokio::test(start_paused = true)]
async fn frame_log_instant_not_started_uses_fallback() {
    let (ack, mut waiter) = PlaybackStartAck::new();
    let fallback = pancetta_core::time::slot_grid_origin();
    ack.not_started();
    assert_eq!(
        frame_log_instant(&mut waiter, fallback, TEXT).await,
        fallback
    );
}
#[tokio::test(start_paused = true)]
async fn frame_log_instant_dropped_uses_fallback() {
    let (ack, mut waiter) = PlaybackStartAck::new();
    let fallback = pancetta_core::time::slot_grid_origin();
    drop(ack);
    assert_eq!(
        frame_log_instant(&mut waiter, fallback, TEXT).await,
        fallback
    );
}
#[tokio::test(start_paused = true)]
async fn frame_log_instant_pending_uses_fallback_after_bound() {
    let (_ack, mut waiter) = PlaybackStartAck::new();
    let fallback = pancetta_core::time::slot_grid_origin();
    let before = tokio::time::Instant::now();
    assert_eq!(
        frame_log_instant(&mut waiter, fallback, TEXT).await,
        fallback
    );
    assert!(before.elapsed() >= TX_PLAYBACK_START_WAIT);
}
#[tokio::test(start_paused = true)]
async fn a_held_playback_start_ack_never_holds_ptt_past_playback() {
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let mut ptt_on = false;
    let mut ptt_off_after_on = false;
    let mut ptt_off_at = None;
    let mut audio = None;
    let mut held = Vec::new();
    let mut completed = None;
    while completed.is_none() && tokio::time::Instant::now() < deadline {
        while let Ok(msg) = audio_rx.try_recv() {
            if let MessageType::AudioOutput {
                samples,
                sample_rate,
                playback_started,
                ..
            } = msg.message_type
            {
                held.push(playback_started.expect("frame ack"));
                let len = Duration::from_secs_f64(samples.len() as f64 / sample_rate as f64);
                audio.get_or_insert((tokio::time::Instant::now(), len));
            }
        }
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if let MessageType::RigControl(RigControlMessage::SetPtt { state }) = msg.message_type {
                if state {
                    ptt_on = true;
                } else if ptt_on {
                    ptt_off_after_on = true;
                    ptt_off_at.get_or_insert(tokio::time::Instant::now());
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
    let off_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !ptt_off_after_on && tokio::time::Instant::now() < off_deadline {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if let MessageType::RigControl(RigControlMessage::SetPtt { state: false }) =
                msg.message_type
            {
                if ptt_on {
                    ptt_off_after_on = true;
                    ptt_off_at.get_or_insert(tokio::time::Instant::now());
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
    let lines = await_tx_log_lines(&h, 1).await;
    assert_eq!(
        lines.len(),
        1,
        "the queued record lands once the disk responds"
    );
    assert_eq!(lines[0].text, TEXT);
}
