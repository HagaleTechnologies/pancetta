use super::pan115_worker_gate_tests::{
    await_completion, await_ptt_on, request, worker_harness, Harness,
};
use crate::message_bus::{ComponentId, ComponentMessage, MessageType, TxOrigin};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// 20m: in-band under the default region-2 plan.
const IN_BAND_DIAL_HZ: u64 = 14_074_000;
/// 15.000 MHz: between 20m and 17m — inside no amateur allocation.
const OUT_OF_BAND_DIAL_HZ: u64 = 15_000_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_band_single_tx_is_refused_before_ptt() {
    let h = worker_harness().await;
    // The shared RX-dial atomic, as the Hamlib poll loop would set it.
    h.coordinator
        .operating_frequency_hz
        .store(OUT_OF_BAND_DIAL_HZ, Ordering::Relaxed);

    request(&h, "CQ N0CALL FN20").await;
    let (completed, diagnostics, ptt_on) =
        await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(5)).await;
    assert_eq!(
        completed,
        Some(false),
        "an out-of-band TX must complete as failed"
    );
    assert!(
        !ptt_on,
        "no SetPtt{{true}} may reach Hamlib for an out-of-band TX"
    );
    assert!(
        diagnostics.iter().any(|d| d.contains(
            "TX blocked (TX frequency 15001500 Hz is outside the configured amateur band plan)"
        )),
        "the admission gate must name the absolute RF, got {diagnostics:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_band_single_tx_still_keys() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);

    request(&h, "CQ N0CALL FN20").await;
    assert!(
        await_ptt_on(&h, Duration::from_secs(40)).await,
        "an in-band TX must still key (guard against over-blocking)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_dial_zero_still_keys() {
    let h = worker_harness().await;
    // Dial never set: the unknown-0 sentinel must skip the band-plan
    // check (mock/rig-disabled dev modes, --test-tx, replay seed).
    request(&h, "CQ N0CALL FN20").await;
    assert!(
        await_ptt_on(&h, Duration::from_secs(40)).await,
        "a TX with an unknown (0) dial must still key"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_out_of_band_is_refused_even_with_in_band_rx_dial() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
    // Split active: the station transmits on the split dial, which is
    // out of band — the gate must resolve the effective TX dial, not
    // the in-band RX dial.
    h.coordinator
        .split_tx_frequency_hz
        .store(OUT_OF_BAND_DIAL_HZ, Ordering::Relaxed);

    request(&h, "CQ N0CALL FN20").await;
    let (completed, diagnostics, ptt_on) =
        await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(5)).await;
    assert_eq!(
        completed,
        Some(false),
        "a split TX landing out of band must complete as failed"
    );
    assert!(!ptt_on, "no SetPtt(true) for an out-of-band split TX");
    assert!(
        diagnostics.iter().any(|d| d.contains(
            "TX blocked (TX frequency 15001500 Hz is outside the configured amateur band plan)"
        )),
        "the gate must name the split-resolved absolute RF, got {diagnostics:?}"
    );
}

/// Park the worker at the PTT mutex, then move the dial out of band.
/// The final pre-PTT check must observe the new dial and refuse to key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dial_moved_out_of_band_before_key_is_refused() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);

    let held = h.coordinator.ptt_sync_gate.lock().await;
    let text = "CQ N0CALL FN20";
    request(&h, text).await;
    tokio::time::timeout(
        Duration::from_secs(40),
        h.coordinator.tx_before_ptt_gate.notified(),
    )
    .await
    .expect("worker must reach the pre-PTT gate before the dial moves");
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
    h.coordinator
        .operating_frequency_hz
        .store(OUT_OF_BAND_DIAL_HZ, Ordering::Relaxed);
    drop(held);

    let (completed, diagnostics, ptt_on) =
        await_completion(&h, text, Duration::from_secs(45)).await;
    assert_eq!(
        completed,
        Some(false),
        "a frame whose dial moved out of band before key must complete as failed"
    );
    assert!(!ptt_on, "no SetPtt(true) after the dial moved out of band");
    assert!(
        diagnostics.iter().any(|d| {
            d.contains("TX blocked at final pre-PTT check")
                && d.contains("outside the configured amateur band plan")
        }),
        "the final pre-PTT gate must report the band-plan refusal, got {diagnostics:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tune_request_out_of_band_is_refused() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(OUT_OF_BAND_DIAL_HZ, Ordering::Relaxed);
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Tui,
            ComponentId::Ft8Transmitter,
            MessageType::TuneRequest {
                duration_secs: 5,
                tone_offset_hz: 1500.0,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_refusal = false;
    let mut ptt_on = false;
    while Instant::now() < deadline && !saw_refusal {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            ptt_on |= matches!(
                msg.message_type,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                    state: true
                })
            );
        }
        while let Ok(msg) = h.tui_rx.try_recv() {
            if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
                if text.contains("outside the configured amateur band plan")
                    && text.contains("tune")
                {
                    saw_refusal = true;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw_refusal,
        "an out-of-band tune must be refused with a diagnostic"
    );
    assert!(!ptt_on, "no SetPtt(true) for an out-of-band tune");

    // Control: the same tune on an in-band dial still keys.
    h.coordinator
        .operating_frequency_hz
        .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Tui,
            ComponentId::Ft8Transmitter,
            MessageType::TuneRequest {
                duration_secs: 2,
                tone_offset_hz: 1500.0,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
    assert!(
        await_ptt_on(&h, Duration::from_secs(10)).await,
        "an in-band tune must still key"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tune_dial_moved_out_of_band_before_ptt_is_refused_at_last_instant() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
    // Hold the PAN-143 PTT sync gate so the worker parks inside
    // `PttGuard::new` — after the tune's admission band-plan check
    // passed on the in-band dial, before the PTT-on send.
    let ptt_sync_gate = h.coordinator.ptt_sync_gate.clone();
    let held = ptt_sync_gate.lock().await;
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Tui,
            ComponentId::Ft8Transmitter,
            MessageType::TuneRequest {
                duration_secs: 1,
                tone_offset_hz: 1500.0,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
    // Wait for the worker to dequeue the request. From there the tune's
    // admission checks and tone build run without yielding until the
    // held gate, so a short settle covers them.
    let (worker_inbox, _) = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !worker_inbox.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        worker_inbox.is_empty(),
        "the TX worker must dequeue the tune request"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut diagnostics = Vec::new();
    while let Ok(msg) = h.tui_rx.try_recv() {
        if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
            diagnostics.push(text);
        }
    }
    assert!(
        diagnostics.iter().all(|d| !d.contains("TX blocked")),
        "the in-band tune must pass admission before the dial moves: {diagnostics:?}"
    );

    // The effective TX dial leaves the band plan while the worker waits.
    h.coordinator
        .operating_frequency_hz
        .store(OUT_OF_BAND_DIAL_HZ, Ordering::Relaxed);
    drop(held);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_refusal = false;
    let mut ptt_on = false;
    while Instant::now() < deadline && !saw_refusal && !ptt_on {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            ptt_on |= matches!(
                msg.message_type,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                    state: true
                })
            );
        }
        while let Ok(msg) = h.tui_rx.try_recv() {
            if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
                saw_refusal |= text.contains("final pre-PTT check")
                    && text.contains("outside the configured amateur band plan")
                    && text.contains("tune");
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !ptt_on,
        "no SetPtt(true) for a tune whose dial left the band plan before PTT"
    );
    assert!(
        saw_refusal,
        "the tune must be refused at the final pre-PTT check with a band-plan diagnostic"
    );
}

/// Send one two-item local bundle through the real worker.
async fn send_bundle(h: &Harness, a_offset: f64, b_offset: f64) {
    use crate::message_bus::TransmitRequestItem;
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
                        frequency_offset: a_offset,
                        qso_id: None,
                        origin: crate::message_bus::TxOrigin::Local,
                        remote_client_key_id: None,
                    },
                    TransmitRequestItem {
                        attempt_id: None,
                        message_text: "CQ N0CALL EN27".to_string(),
                        frequency_offset: b_offset,
                        qso_id: None,
                        origin: crate::message_bus::TxOrigin::Local,
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
}

/// Drain until the TransmitComplete for EVERY text in `texts` has
/// arrived on Autonomous (bundles complete per item, back-to-back — a
/// single-text waiter would discard its siblings' completions while
/// polling), returning each text's `success` in order plus whether any
/// `SetPtt{true}` reached Hamlib.
async fn await_bundle_completions(
    h: &Harness,
    texts: &[&str],
    within: Duration,
) -> (Vec<Option<bool>>, bool) {
    let deadline = Instant::now() + within;
    let mut ptt_on = false;
    let mut done: Vec<Option<bool>> = vec![None; texts.len()];
    while Instant::now() < deadline && done.iter().any(|d| d.is_none()) {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            ptt_on |= matches!(
                msg.message_type,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                    state: true
                })
            );
        }
        while let Ok(msg) = h.autonomous_rx.try_recv() {
            if let MessageType::TransmitComplete {
                success,
                message_text,
                ..
            } = msg.message_type
            {
                if let Some(slot) = texts.iter().position(|t| *t == message_text) {
                    if done[slot].is_none() {
                        done[slot] = Some(success);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    while let Ok(msg) = h.hamlib_rx.try_recv() {
        ptt_on |= matches!(
            msg.message_type,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state: true })
        );
    }
    (done, ptt_on)
}

/// PAN-118 Gherkin, bundle discrimination: at the 20m upper edge
/// (14.348 MHz dial), a +500 Hz item lands in-band while a +2,500 Hz
/// sibling lands past the edge (14.3505 MHz) — only the offending
/// item is dropped; the in-privilege sibling still keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundle_drops_only_the_out_of_band_item() {
    let h = worker_harness().await;
    // 20m region-2 upper edge is 14.350 MHz.
    h.coordinator
        .operating_frequency_hz
        .store(14_348_000, Ordering::Relaxed);

    send_bundle(&h, 500.0, 2_500.0).await;
    // The out-of-band item is dropped at admission with a failed
    // completion naming its absolute RF.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_drop_diag = false;
    while Instant::now() < deadline && !saw_drop_diag {
        saw_drop_diag |= h
            .tui_rx
            .try_iter()
            .any(|msg| matches!(msg.message_type, MessageType::DiagnosticEvent { text, .. } if text.contains("dropping out-of-band multi-TX item") && text.contains("14350500")));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        saw_drop_diag,
        "the dropped item's diagnostic must name 14350500 Hz"
    );
    let (done, ptt_on) = await_bundle_completions(
        &h,
        &["CQ N0CALL FN20", "CQ N0CALL EN27"],
        Duration::from_secs(50),
    )
    .await;
    assert_eq!(
        done[1],
        Some(false),
        "the out-of-band item must complete as failed"
    );
    assert_eq!(
        done[0],
        Some(true),
        "the in-band sibling must complete successfully"
    );
    assert!(ptt_on, "the in-band sibling must key PTT");
}

/// Control: both offsets in-band on a normal 20m dial — both items key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundle_fully_in_band_keys_all_items() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(14_074_000, Ordering::Relaxed);

    send_bundle(&h, 500.0, 2_500.0).await;
    let (done, ptt_on) = await_bundle_completions(
        &h,
        &["CQ N0CALL FN20", "CQ N0CALL EN27"],
        Duration::from_secs(50),
    )
    .await;
    assert_eq!(done[0], Some(true), "item A must complete successfully");
    assert_eq!(done[1], Some(true), "item B must complete successfully");
    assert!(ptt_on, "the bundle must key");
}

/// Every item out of privilege (dial between bands): the whole bundle
/// is refused — nothing keys, both items fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundle_fully_out_of_band_keys_nothing() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(OUT_OF_BAND_DIAL_HZ, Ordering::Relaxed);

    send_bundle(&h, 500.0, 2_500.0).await;
    let (done, ptt_on) = await_bundle_completions(
        &h,
        &["CQ N0CALL FN20", "CQ N0CALL EN27"],
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(done[0], Some(false), "item A must complete as failed");
    assert_eq!(done[1], Some(false), "item B must complete as failed");
    assert!(!ptt_on, "no SetPtt(true) for a fully out-of-band bundle");
}
