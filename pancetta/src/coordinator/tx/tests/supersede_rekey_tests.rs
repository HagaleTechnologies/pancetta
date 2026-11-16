use super::*;
use crate::message_bus::MessageBus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// PAN-273: the supersede helpers unkey through the caller's guard, so
/// tests hand them one keyed the way the worker's arm does.
pub(super) async fn keyed_guard(
    bus: &MessageBus,
    ptt_active: &Arc<AtomicBool>,
    last_ptt_on_ms: &Arc<AtomicU64>,
    last_ptt_off_ms: &Arc<AtomicU64>,
) -> PttGuard {
    PttGuard::new(
        bus.clone(),
        ptt_active.clone(),
        &Arc::new(tokio::sync::Mutex::new(())),
        last_ptt_on_ms,
        last_ptt_off_ms,
        &PendingPttOff::default(),
    )
    .await
}

fn ptt_offs(rx: &crossbeam_channel::Receiver<ComponentMessage>) -> usize {
    rx.try_iter()
        .filter(|m| {
            matches!(
                m.message_type,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                    state: false
                })
            )
        })
        .count()
}

fn viable_single_replace(now: chrono::DateTime<chrono::Utc>) -> (MessageType, TxSchedule) {
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );
    let schedule = super::schedule_tx(
        now,
        cur_parity,
        20_000,
        12_000,
        pancetta_core::slot::SLOT_NS,
    );
    let new_request = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "NEW TEXT".to_string(),
        frequency_offset: 1500.0,
        qso_id: Some("qso-1".to_string()),
        tx_parity: Some(cur_parity),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    (new_request, schedule)
}

/// PAN-273 scenario 1 at the supersede helper: Hamlib's channel is full
/// for the whole release window. The re-key decision still stands, but
/// the guard stays armed and keyed (no PTT-off was queued), and its
/// `Drop` delivers the unkey once Hamlib catches up. Before, the helper
/// returned `Replace` with `ptt_active=false` and nothing delivered.
#[tokio::test(flavor = "current_thread")]
async fn supersede_into_a_full_hamlib_channel_keeps_the_guard_armed() {
    let bus = MessageBus::new(1).unwrap();
    let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    assert!(bus
        .send_message_checked(ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::AudioData(Vec::new()),
            Instant::now(),
        ))
        .await
        .unwrap());
    let now = chrono::Utc::now();
    let (new_request, mut schedule) = viable_single_replace(now);
    let mut message_text = "OLD TEXT".to_string();
    let mut frequency_offset = 1000.0;
    let ptt_active = Arc::new(AtomicBool::new(false));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));
    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;

    // Pin the clock domain: the helper must resolve the same phase the
    // request was built at, not whatever the real clock says mid-test.
    let mut timing = super::SlotTimingContext::ft8_for_test(
        20_000,
        12_000,
        pancetta_core::slot::SLOT_NS,
        pancetta_config::station::TxSelfParity::Auto,
        now,
    );
    timing.utc_clock = Arc::new(move || now);

    let outcome = super::supersede_and_rekey_or_bundle(
        new_request,
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
        },
        &mut guard,
        &timing,
        1,
        &super::SupersedeInFlight {
            items: &[],
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: None,
        },
    )
    .await;

    assert!(matches!(outcome, super::SupersedeOutcome::Replace { .. }));
    assert!(guard.armed, "an undelivered unkey keeps the fallback armed");
    assert!(
        ptt_active.load(Ordering::Acquire),
        "no PTT-off was queued: the rig is still keyed"
    );
    assert_eq!(ptt_offs(&hamlib_rx), 0, "the channel was full throughout");

    // Hamlib catches up; the armed guard's Drop delivers the unkey.
    drop(guard);
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        ptt_offs(&hamlib_rx),
        1,
        "the guard's Drop unkeys exactly once"
    );
    assert!(!ptt_active.load(Ordering::Acquire));
}

/// PAN-273: room frees inside the release window, so the supersede's own
/// retry queues the PTT-off and stands the guard down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supersede_retries_ptt_off_until_hamlib_frees_room() {
    let bus = MessageBus::new(1).unwrap();
    let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    assert!(bus
        .send_message_checked(ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::AudioData(Vec::new()),
            Instant::now(),
        ))
        .await
        .unwrap());
    let drain = hamlib_rx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let _ = drain.try_recv();
    });
    let now = pancetta_core::time::slot_grid_origin();
    let (new_request, mut schedule) = viable_single_replace(now);
    let mut message_text = "OLD TEXT".to_string();
    let mut frequency_offset = 1000.0;
    let ptt_active = Arc::new(AtomicBool::new(false));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));
    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;

    // The real Hamlib retry wait must not change the scheduling phase.
    let mut timing = super::SlotTimingContext::ft8_for_test(
        20_000,
        12_000,
        pancetta_core::slot::SLOT_NS,
        pancetta_config::station::TxSelfParity::Auto,
        now,
    );
    timing.utc_clock = Arc::new(move || now);

    let outcome = super::supersede_and_rekey_or_bundle(
        new_request,
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
        },
        &mut guard,
        &timing,
        1,
        &super::SupersedeInFlight {
            items: &[],
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: None,
        },
    )
    .await;

    assert!(matches!(outcome, super::SupersedeOutcome::Replace { .. }));
    assert_eq!(ptt_offs(&hamlib_rx), 1, "the retry queued the unkey");
    assert!(!guard.armed, "a delivered unkey stands the guard down");
    assert!(!ptt_active.load(Ordering::Acquire));
    assert!(last_ptt_off_ms.load(Ordering::Acquire) > 0);
}

/// The core Phase-1 re-key decision: a genuinely different superseding
/// request that can still make this slot must deassert PTT and swap the
/// working `message_text`/`frequency_offset`/`schedule` in place so the
/// caller's `'key_and_send` retry re-encodes (Step 1) and re-keys with the
/// NEW content. Targets the current slot's parity with a >1-slot
/// `tx_late_max_ms` so the re-key is viable no matter when the test runs.
#[tokio::test(flavor = "current_thread")]
async fn supersede_and_rekey_updates_state_when_viable() {
    let bus = MessageBus::new(16).unwrap();
    // Hamlib channel so the helper's PTT-OFF send has a receiver.
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );

    let mut message_text = "OLD TEXT".to_string();
    let mut frequency_offset = 1000.0;
    let mut schedule = super::schedule_tx(
        now,
        cur_parity,
        20_000,
        12_000,
        pancetta_core::slot::SLOT_NS,
    );
    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

    let new_request = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "NEW TEXT".to_string(),
        frequency_offset: 1500.0,
        qso_id: Some("qso-1".to_string()),
        tx_parity: Some(cur_parity),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };

    // max_concurrent_qsos == 1 → single-item replace (Task 6 behavior).
    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    let outcome = super::supersede_and_rekey_or_bundle(
        new_request,
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
        },
        &mut guard,
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
        ),
        1,
        &super::SupersedeInFlight {
            items: &[],
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: None,
        },
    )
    .await;

    assert!(
        matches!(outcome, super::SupersedeOutcome::Replace { .. }),
        "re-key targeting the current slot parity with max_concurrent_qsos==1 must Replace"
    );
    // The state swap is what makes the retry re-encode the NEW content:
    // Step 1 re-runs `encode_for_protocol(&message_text)` at the new freq.
    assert_eq!(message_text, "NEW TEXT");
    assert_eq!(frequency_offset, 1500.0);
    assert!(
        !ptt_active.load(Ordering::Acquire),
        "PTT must be deasserted"
    );
    assert!(
        !schedule.deferred,
        "a viable re-key schedule is never deferred"
    );
}

/// TASK 9 — the end-to-end audio gap Task 6 explicitly deferred.
///
/// Task 6's `supersede_and_rekey_updates_state_when_viable` proves only the
/// PRECONDITION of a single-TX supersede: PTT is deasserted and the working
/// `message_text`/`frequency_offset` are swapped to the new request. It does
/// NOT prove the crux — that the audio the single-TX arm actually
/// re-encodes and pushes to the output ring buffer (Step 1 → Step 7)
/// reflects the NEW message's waveform rather than stale samples from the
/// aborted one. Task 6's reviewer flagged that as deferred here.
///
/// This test closes it by driving the REAL single-TX supersede helper
/// (`supersede_and_rekey_or_bundle`, `max_concurrent_qsos == 1` →
/// `Replace`) over a REAL `MessageBus`, then re-encoding the POST-supersede
/// working state through `encode_and_modulate` — documented (see its
/// doc-comment's "FT8 regression guarantee") to be byte-identical to the
/// arm's Step-1 `encode_for_protocol` + `modulate_for_protocol` sequence for
/// a given (text, offset). It asserts:
///
///   1. The abort of the in-flight message A is a REAL
///      `SetPtt{false}` observed on a REAL Hamlib channel (not just the
///      `ptt_active` flag), and it is the ONLY message on that channel (no
///      stray/duplicate keying).
///   2. The waveform re-encoded from the mutated working state is
///      BIT-IDENTICAL to a fresh encode of B and DIFFERS from A's waveform —
///      i.e. the audio the arm's Step 7 would send is B's content, not A's
///      stale samples. This is the exact gap Task 6 left open.
///   3. That waveform is a full-length FT8 frame (same length as A's), so
///      the difference is genuine re-encoded CONTENT, not a
///      truncated/empty buffer.
///
/// NOT covered (helper-level scope; see task-9-report.md): PTT going back ON
/// and the single `TransmitComplete` for B are emitted by the arm's inline
/// Steps 5/10, which are not separable from `start_transmitter_component`
/// without a much larger worker harness than this task warrants.
#[tokio::test(flavor = "current_thread")]
async fn supersede_rekeys_audio_to_new_message_not_stale() {
    let bus = MessageBus::new(16).unwrap();
    // Real Hamlib channel: the helper's abort PTT-OFF lands here.
    let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );

    // Message A is "in flight": these are the working-loop variables the
    // arm's Step 1 would encode into the audio it is currently sending.
    let a_text = "CQ K5ARH EM12";
    let a_freq = 1000.0_f64;
    let mut message_text = a_text.to_string();
    let mut frequency_offset = a_freq;
    let mut schedule = super::schedule_tx(
        now,
        cur_parity,
        20_000,
        12_000,
        pancetta_core::slot::SLOT_NS,
    );
    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

    // A genuinely different superseding message B — different text AND
    // frequency, so its encoded waveform is unmistakably distinct from A's.
    let b_text = "W1AW K5ARH R-09";
    let b_freq = 1600.0_f64;
    let new_request = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: b_text.to_string(),
        frequency_offset: b_freq,
        qso_id: Some("qso-super".to_string()),
        tx_parity: Some(cur_parity),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };

    // max_concurrent_qsos == 1 → the single-TX arm's `Replace` path.
    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    let outcome = super::supersede_and_rekey_or_bundle(
        new_request,
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
        },
        &mut guard,
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
        ),
        1,
        &super::SupersedeInFlight {
            items: &[],
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: None,
        },
    )
    .await;

    assert!(
        matches!(outcome, super::SupersedeOutcome::Replace { .. }),
        "a viable single-TX supersede must Replace"
    );

    // (1) The abort of A is a REAL SetPtt{false} on the real Hamlib channel.
    assert!(
        !ptt_active.load(Ordering::Acquire),
        "ptt_active flag must be cleared by the abort"
    );
    let ptt_msg = hamlib_rx
        .try_recv()
        .expect("aborting the in-flight message must send a real SetPtt to Hamlib");
    assert!(
        matches!(
            ptt_msg.message_type,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state: false })
        ),
        "abort must be a real SetPtt{{false}}, got {:?}",
        ptt_msg.message_type
    );
    // Exactly one keying message from the abort — no stray/duplicate PTT.
    assert!(
        hamlib_rx.try_recv().is_err(),
        "the abort emits exactly one SetPtt{{false}} (no duplicate keying)"
    );

    // (2) THE CRUX: encode the POST-supersede working state exactly as the
    // arm's Step 1 would, and prove it is B's waveform, not A's stale one.
    // `arm_encode` reproduces the single-TX arm's Step-1 keying byte for
    // byte: `set_base_frequency(freq)` then `encode_for_protocol` +
    // `modulate_for_protocol(.., 0.0)` (see tx.rs ~2516-2564). Using the
    // arm's real sequence — rather than the `encode_and_modulate` helper,
    // which routes the frequency through the modulator's default base
    // instead of `set_base_frequency` — makes "this is the waveform the arm
    // sends" a faithful claim.
    let protocol = pancetta_ft8::Protocol::Ft8;
    let arm_encode = |text: &str, freq: f64| -> Vec<f32> {
        let mut encoder = Ft8Encoder::new();
        let mut modulator = Ft8Modulator::new_default().expect("modulator");
        modulator
            .set_base_frequency(freq)
            .expect("set base frequency");
        let symbols =
            super::encode_for_protocol(&mut encoder, protocol, text).expect("encode symbols");
        super::modulate_for_protocol(&mut modulator, protocol, &symbols, 0.0).expect("modulate")
    };

    let rekeyed_audio = arm_encode(&message_text, frequency_offset);
    let fresh_b = arm_encode(b_text, b_freq);
    let fresh_a = arm_encode(a_text, a_freq);

    assert_eq!(
        rekeyed_audio, fresh_b,
        "the audio the arm's Step 7 would push must be B's freshly-encoded \
         waveform (the working state re-encodes to the NEW message)"
    );
    assert_ne!(
        rekeyed_audio, fresh_a,
        "the audio must NOT be A's stale waveform — this is the exact \
         end-to-end gap Task 6 deferred to Task 9"
    );

    // (3) The re-encode is a genuine full FT8 frame, not truncated/empty:
    // same length as A's frame, so the mismatch above is CONTENT, not size.
    assert!(
        !rekeyed_audio.is_empty(),
        "re-encoded audio must be a real, non-empty frame"
    );
    assert_eq!(
        rekeyed_audio.len(),
        fresh_a.len(),
        "same full FT8-frame length as A — the A/B waveform difference is \
         re-encoded content, not a truncated or empty buffer"
    );
}

/// With `max_concurrent_qsos > 1`, a viable supersede folds the new
/// request into a bundle alongside the in-flight item: the outcome is
/// `Bundle` carrying both items (in-flight first, new item last), PTT is
/// deasserted, and the working state is ALSO mutated to the new item (the
/// single-TX caller's collision fallback).
#[tokio::test(flavor = "current_thread")]
async fn supersede_returns_bundle_when_concurrent() {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );

    let mut message_text = "OLD TEXT".to_string();
    let mut frequency_offset = 1000.0;
    let mut schedule = super::schedule_tx(
        now,
        cur_parity,
        20_000,
        12_000,
        pancetta_core::slot::SLOT_NS,
    );
    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

    let in_flight = [crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: "KA1ABC K5ARH R-15".to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("qso-1".to_string()),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }];

    let new_request = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "CQ K5ARH EM12".to_string(),
        frequency_offset: 1400.0,
        qso_id: Some("qso-2".to_string()),
        tx_parity: Some(cur_parity),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };

    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    let outcome = super::supersede_and_rekey_or_bundle(
        new_request,
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
        },
        &mut guard,
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
        ),
        2,
        &super::SupersedeInFlight {
            items: &in_flight,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: None,
        },
    )
    .await;

    match outcome {
        super::SupersedeOutcome::Bundle { items, .. } => {
            assert_eq!(items.len(), 2, "bundle = in-flight + new item");
            assert_eq!(items[0].message_text, "KA1ABC K5ARH R-15");
            assert_eq!(items[0].frequency_offset, 1000.0);
            assert_eq!(items[1].message_text, "CQ K5ARH EM12");
            assert_eq!(items[1].frequency_offset, 1400.0);
            assert_eq!(items[1].qso_id.as_deref(), Some("qso-2"));
        }
        other => panic!("expected Bundle, got {other:?}"),
    }
    assert!(
        !ptt_active.load(Ordering::Acquire),
        "PTT must be deasserted"
    );
    // Working state also mutated to the new item (collision fallback).
    assert_eq!(message_text, "CQ K5ARH EM12");
    assert_eq!(frequency_offset, 1400.0);
}

/// PAN-98: a station-agent-bound in-flight stream (`Some` identity) and a
/// superseding WSJT-X UDP request (`None` identity) — or the reverse —
/// are different channels with independent consent. Folding them would
/// tag the bundle with ONE identity, so its key-time gate would check
/// only one channel's consent and the other channel's frame would key by
/// proxy. This must produce an identity-conflict `Replace`, never
/// `Bundle`, in both directions.
#[tokio::test]
async fn supersede_refuses_to_bundle_across_consent_channels() {
    for (in_flight_id, new_id) in [(Some("client-a"), None), (None, Some("client-a"))] {
        let bus = MessageBus::new(16).unwrap();
        let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

        let now = chrono::Utc::now();
        let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
            pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
            pancetta_core::slot::SLOT_NS,
        );

        let mut message_text = "OLD TEXT".to_string();
        let mut frequency_offset = 1000.0;
        let mut schedule = super::schedule_tx(
            now,
            cur_parity,
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
        );
        let ptt_active = Arc::new(AtomicBool::new(true));
        let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
        let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

        let in_flight = [crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Remote,
            remote_client_key_id: in_flight_id.map(str::to_string),
        }];

        let new_request = MessageType::TransmitRequest {
            attempt_id: None,
            message_text: "CQ K5ARH EM12".to_string(),
            frequency_offset: 1400.0,
            qso_id: Some("qso-2".to_string()),
            tx_parity: Some(cur_parity),
            origin: crate::message_bus::TxOrigin::Remote,
            remote_client_key_id: new_id.map(str::to_string),
        };

        let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
        let outcome = super::supersede_and_rekey_or_bundle(
            new_request,
            super::WorkingFrame {
                message_text: &mut message_text,
                frequency_offset: &mut frequency_offset,
                schedule: &mut schedule,
            },
            &mut guard,
            &super::SlotTimingContext::ft8_for_test(
                20_000,
                12_000,
                pancetta_core::slot::SLOT_NS,
                pancetta_config::station::TxSelfParity::Auto,
                now,
            ),
            2,
            &super::SupersedeInFlight {
                items: &in_flight,
                origin: crate::message_bus::TxOrigin::Remote,
                remote_client_key_id: in_flight_id.map(str::to_string),
                tx_parity: None,
            },
        )
        .await;

        match outcome {
            super::SupersedeOutcome::Replace {
                remote_client_key_id,
                identity_conflict,
                ..
            } => {
                assert_eq!(
                    remote_client_key_id.as_deref(),
                    new_id,
                    "Replace must carry the NEW request's own identity (channel)"
                );
                assert!(
                    identity_conflict,
                    "a cross-channel Replace must be flagged as a conflict"
                );
            }
            other => panic!(
                "cross-channel in-flight/new streams ({in_flight_id:?} / {new_id:?}) \
                 must never fold into a Bundle, got {other:?}"
            ),
        }
    }
}

/// PAN-136: a supersede bundle gives the newly added item the NEW
/// request's own binding (not a hard-coded `Local`), keeps the in-flight
/// item's binding, and still gates the bundle under the fold of both.
#[tokio::test]
async fn supersede_bundle_gives_the_new_item_the_new_requests_binding() {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );

    let mut message_text = "OLD TEXT".to_string();
    let mut frequency_offset = 1000.0;
    let mut schedule = super::schedule_tx(
        now,
        cur_parity,
        20_000,
        12_000,
        pancetta_core::slot::SLOT_NS,
    );
    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

    let in_flight = [crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: "KA1ABC K5ARH R-15".to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("qso-1".to_string()),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }];

    let new_request = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "CQ K5ARH EM12".to_string(),
        frequency_offset: 1400.0,
        qso_id: Some("qso-2".to_string()),
        tx_parity: Some(cur_parity),
        origin: crate::message_bus::TxOrigin::Remote,
        remote_client_key_id: Some("client-a".to_string()),
    };

    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    let outcome = super::supersede_and_rekey_or_bundle(
        new_request,
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
        },
        &mut guard,
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
        ),
        2,
        &super::SupersedeInFlight {
            items: &in_flight,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: None,
        },
    )
    .await;

    match outcome {
        super::SupersedeOutcome::Bundle {
            items,
            bundle_origin,
            bundle_remote_client_key_id,
            ..
        } => {
            assert_eq!(items.len(), 2);
            let new_item = items.last().unwrap();
            assert_eq!(new_item.qso_id.as_deref(), Some("qso-2"));
            assert_eq!(new_item.origin, crate::message_bus::TxOrigin::Remote);
            assert_eq!(new_item.remote_client_key_id.as_deref(), Some("client-a"));
            let kept = &items[0];
            assert_eq!(kept.qso_id.as_deref(), Some("qso-1"));
            assert_eq!(kept.origin, crate::message_bus::TxOrigin::Local);
            assert_eq!(kept.remote_client_key_id, None);
            assert_eq!(bundle_origin, crate::message_bus::TxOrigin::Remote);
            assert_eq!(bundle_remote_client_key_id.as_deref(), Some("client-a"));
        }
        other => panic!("a Local in-flight + Remote new request must bundle, got {other:?}"),
    }
}

/// PAN-91 review follow-up: an in-flight stream bound to one remote
/// client and a superseding request bound to a DIFFERENT remote client
/// must NEVER fold into one bundle — a single bundle-level identity
/// check would then authorize the OTHER client's frame by proxy. This
/// must produce `Replace` (abandon the in-flight side, key only the new
/// request under its OWN identity), never `Bundle`.
#[tokio::test]
async fn supersede_refuses_to_bundle_mismatched_remote_clients() {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );

    let mut message_text = "OLD TEXT".to_string();
    let mut frequency_offset = 1000.0;
    let mut schedule = super::schedule_tx(
        now,
        cur_parity,
        20_000,
        12_000,
        pancetta_core::slot::SLOT_NS,
    );
    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

    let in_flight = [crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: "KA1ABC K5ARH R-15".to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("qso-1".to_string()),
        origin: crate::message_bus::TxOrigin::Remote,
        remote_client_key_id: Some("client-a".to_string()),
    }];

    let new_request = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "CQ K5ARH EM12".to_string(),
        frequency_offset: 1400.0,
        qso_id: Some("qso-2".to_string()),
        tx_parity: Some(cur_parity),
        origin: crate::message_bus::TxOrigin::Remote,
        remote_client_key_id: Some("client-b".to_string()),
    };

    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    let outcome = super::supersede_and_rekey_or_bundle(
        new_request,
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
        },
        &mut guard,
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
        ),
        2,
        &super::SupersedeInFlight {
            items: &in_flight,
            origin: crate::message_bus::TxOrigin::Remote,
            remote_client_key_id: Some("client-a".to_string()),
            tx_parity: None,
        },
    )
    .await;

    match outcome {
        super::SupersedeOutcome::Replace {
            origin,
            remote_client_key_id,
            qso_id,
            identity_conflict,
            tx_parity,
            attempt_id: _,
        } => {
            assert_eq!(origin, crate::message_bus::TxOrigin::Remote);
            assert_eq!(
                remote_client_key_id.as_deref(),
                Some("client-b"),
                "Replace must carry the NEW request's own identity, not the \
                 abandoned in-flight stream's"
            );
            assert_eq!(qso_id.as_deref(), Some("qso-2"));
            assert!(
                identity_conflict,
                "a mismatched-remote-client Replace must be flagged as an \
                 identity conflict, not treated as an ordinary over-capacity \
                 fallback"
            );
            assert_eq!(
                tx_parity,
                Some(cur_parity),
                "Replace must carry the NEW request's own tx_parity"
            );
        }
        other => panic!(
            "mismatched-client in-flight/new streams must never fold into a \
             Bundle, got {other:?}"
        ),
    }
    // Working state mutated to the new item, exactly like the ordinary
    // collision-fallback Replace path.
    assert_eq!(message_text, "CQ K5ARH EM12");
    assert_eq!(frequency_offset, 1400.0);
}

/// Codex round 5 (PR #346): a same-QSO, different-frequency candidate
/// (e.g. a stuck-DX collision-avoidance hop — reachable here since round
/// 3 made the same-QSO duplicate check frequency-aware) must REPLACE the
/// matching in-flight bundle item, not sit alongside it. Appending a
/// second entry for the same QSO would transmit both the stale
/// old-frequency frame and the fresh new-frequency frame at once,
/// defeating the entire purpose of moving off the colliding frequency.
#[tokio::test]
async fn supersede_bundle_replaces_matching_qso_item_on_frequency_hop() {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );

    let mut message_text = "OLD TEXT".to_string();
    let mut frequency_offset = 1000.0;
    let mut schedule = super::schedule_tx(
        now,
        cur_parity,
        20_000,
        12_000,
        pancetta_core::slot::SLOT_NS,
    );
    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

    // Two in-flight QSOs; qso-1 is about to hop off 1000.0 Hz.
    let in_flight = [
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "OTHER W5AU R-08".to_string(),
            frequency_offset: 1400.0,
            qso_id: Some("qso-other".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
    ];

    // Same text, same QSO, hopped 300 Hz off — the stuck-DX collision
    // avoidance shape.
    let new_request = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "KA1ABC K5ARH R-15".to_string(),
        frequency_offset: 1300.0,
        qso_id: Some("qso-1".to_string()),
        tx_parity: Some(cur_parity),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };

    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    let outcome = super::supersede_and_rekey_or_bundle(
        new_request,
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
        },
        &mut guard,
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
        ),
        2,
        &super::SupersedeInFlight {
            items: &in_flight,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: None,
        },
    )
    .await;

    match outcome {
        super::SupersedeOutcome::Bundle { items, .. } => {
            assert_eq!(
                items.len(),
                2,
                "qso-1's old-frequency entry must be REPLACED, not appended alongside — \
                 bundle should still hold exactly qso-1 (hopped) + qso-other, got {items:?}"
            );
            let qso1 = items
                .iter()
                .find(|it| it.qso_id.as_deref() == Some("qso-1"))
                .expect("qso-1 must still be present");
            assert_eq!(
                qso1.frequency_offset, 1300.0,
                "qso-1's entry must carry the HOPPED frequency, not the stale one"
            );
            assert!(
                !items.iter().any(
                    |it| it.qso_id.as_deref() == Some("qso-1") && it.frequency_offset == 1000.0
                ),
                "the stale old-frequency entry for qso-1 must not survive alongside the hop"
            );
            let other = items
                .iter()
                .find(|it| it.qso_id.as_deref() == Some("qso-other"))
                .expect("qso-other must be untouched");
            assert_eq!(other.frequency_offset, 1400.0);
        }
        other => panic!("expected Bundle, got {other:?}"),
    }
}

/// With `max_concurrent_qsos == 2` and 2 DISTINCT QSOs already in flight,
/// a genuinely NEW (not a same-QSO refresh) 3rd stream must NOT be folded
/// into a 3-item bundle — that would silently exceed the operator's
/// configured concurrency cap. It must fall back to `Replace`, the same
/// fallback already used for a frequency collision.
#[tokio::test(flavor = "current_thread")]
async fn supersede_falls_back_to_replace_when_bundle_would_exceed_cap() {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );

    let mut message_text = "OLD TEXT".to_string();
    let mut frequency_offset = 1000.0;
    let mut schedule = super::schedule_tx(
        now,
        cur_parity,
        20_000,
        12_000,
        pancetta_core::slot::SLOT_NS,
    );
    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

    // Already 2 distinct in-flight QSOs — the configured cap.
    let in_flight = [
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "OTHER W5AU R-08".to_string(),
            frequency_offset: 1400.0,
            qso_id: Some("qso-other".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
    ];

    // A THIRD, genuinely new QSO — not a refresh of either in-flight one.
    let new_request = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "THIRD W5AU EM10".to_string(),
        frequency_offset: 1800.0,
        qso_id: Some("qso-third".to_string()),
        tx_parity: Some(cur_parity),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };

    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    let outcome = super::supersede_and_rekey_or_bundle(
        new_request,
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
        },
        &mut guard,
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
        ),
        2,
        &super::SupersedeInFlight {
items: &in_flight,
origin: // max_concurrent_qsos — already met by the 2 in-flight items
        crate::message_bus::TxOrigin::Local,
remote_client_key_id: None,
tx_parity: None,
},
    )
    .await;

    match outcome {
        super::SupersedeOutcome::Replace {
            qso_id,
            identity_conflict,
            ..
        } => {
            assert_eq!(
                qso_id.as_deref(),
                Some("qso-third"),
                "the Replace must carry the NEW request's own qso_id"
            );
            assert!(
                !identity_conflict,
                "an over-capacity Replace (same client on both sides) must not be \
                 flagged as an identity conflict"
            );
        }
        other => panic!(
            "expected Replace (bundling a 3rd stream would exceed max_concurrent_qsos=2), got {other:?}"
        ),
    }
    // Working state is mutated to the new (3rd) request either way.
    assert_eq!(message_text, "THIRD W5AU EM10");
    assert_eq!(frequency_offset, 1800.0);
}

/// PR #348 review (Codex P2): a manual/untracked in-flight item
/// (`qso_id == None`) must NOT count against `max_concurrent_qsos` —
/// only QSO-keyed streams are concurrent QSOs. With cap=2, one tracked
/// QSO + one manual item already in flight, a SECOND new manual item
/// must still fold into a 3-item bundle (1 QSO-keyed + 2 manual), not
/// be rejected as a "three-QSO" overflow.
#[tokio::test(flavor = "current_thread")]
async fn supersede_bundle_cap_does_not_count_manual_items() {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );

    let mut message_text = "OLD TEXT".to_string();
    let mut frequency_offset = 1000.0;
    let mut schedule = super::schedule_tx(
        now,
        cur_parity,
        20_000,
        12_000,
        pancetta_core::slot::SLOT_NS,
    );
    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

    // One tracked QSO + one manual (untracked) item already in flight.
    let in_flight = [
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "MANUAL TUNE".to_string(),
            frequency_offset: 1400.0,
            qso_id: None,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
    ];

    // A second new manual item — untracked, so `qso_id == None`.
    let new_request = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "SECOND MANUAL".to_string(),
        frequency_offset: 1800.0,
        qso_id: None,
        tx_parity: Some(cur_parity),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };

    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    let outcome = super::supersede_and_rekey_or_bundle(
        new_request,
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
        },
        &mut guard,
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
        ),
        2,
        &super::SupersedeInFlight {
items: &in_flight,
origin: // max_concurrent_qsos — met by qso-1 alone; the 2 manual items must not count
        crate::message_bus::TxOrigin::Local,
remote_client_key_id: None,
tx_parity: None,
},
    )
    .await;

    match outcome {
        super::SupersedeOutcome::Bundle { items, .. } => {
            assert_eq!(
                items.len(),
                3,
                "1 QSO-keyed + 2 manual items must all bundle — manual items don't count \
                 against max_concurrent_qsos"
            );
        }
        other => panic!(
            "expected Bundle (only 1 QSO-keyed stream is in flight, well under the cap of 2), got {other:?}"
        ),
    }
}

/// C1 REGRESSION: the `Replace` outcome must carry the SUPERSEDING request's
/// OWN `origin`, independent of the aborted in-flight transmission's origin.
///
/// The single-TX arm re-keys a `Replace` in place (`continue 'key_and_send`)
/// and re-runs Step 4b-arm's key-time arm gate against its `origin` binding.
/// Before the fix the superseding request's origin was discarded (`..`) and
/// the arm reused the ORIGINAL loop origin, so:
///   - a `Remote` request superseding a `Local` in-flight frame re-keyed
///     under the stale `Local` origin and SKIPPED the arm gate entirely
///     (remote content transmitted with zero arm gating — the C1 leak), and
///   - a `Local` request superseding a `Remote` in-flight frame kept gating
///     a purely local frame (wrongly droppable if the arm went stale).
///
/// Proving `Replace{origin}` equals the NEW request's origin in BOTH
/// directions is exactly what closes both failures: the arm now re-gates and
/// emits the re-keyed frame under the origin of the frame actually going out.
#[tokio::test(flavor = "current_thread")]
async fn supersede_replace_carries_superseding_requests_origin() {
    use crate::message_bus::TxOrigin;

    // (in-flight origin, superseding request origin) — both directions.
    for (in_flight_origin, new_origin) in [
        (TxOrigin::Local, TxOrigin::Remote),
        (TxOrigin::Remote, TxOrigin::Local),
    ] {
        let bus = MessageBus::new(16).unwrap();
        let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

        let now = chrono::Utc::now();
        let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
            pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
            pancetta_core::slot::SLOT_NS,
        );

        let mut message_text = "OLD TEXT".to_string();
        let mut frequency_offset = 1000.0;
        let mut schedule = super::schedule_tx(
            now,
            cur_parity,
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
        );
        let ptt_active = Arc::new(AtomicBool::new(true));
        let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
        let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

        let new_request = MessageType::TransmitRequest {
            attempt_id: None,
            message_text: "NEW TEXT".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-new".to_string()),
            tx_parity: Some(cur_parity),
            origin: new_origin,
            remote_client_key_id: None,
        };

        // max_concurrent_qsos == 1 → the single-TX arm's Replace path.
        let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
        let outcome = super::supersede_and_rekey_or_bundle(
            new_request,
            super::WorkingFrame {
                message_text: &mut message_text,
                frequency_offset: &mut frequency_offset,
                schedule: &mut schedule,
            },
            &mut guard,
            &super::SlotTimingContext::ft8_for_test(
                20_000,
                12_000,
                pancetta_core::slot::SLOT_NS,
                pancetta_config::station::TxSelfParity::Auto,
                now,
            ),
            1,
            &super::SupersedeInFlight {
                items: &[],
                origin: in_flight_origin,
                remote_client_key_id: None,
                tx_parity: None,
            },
        )
        .await;

        match outcome {
            super::SupersedeOutcome::Replace {
                origin,
                qso_id,
                remote_client_key_id: _,
                identity_conflict: _,
                tx_parity: _,
                attempt_id: _,
            } => {
                assert_eq!(
                    origin, new_origin,
                    "Replace must carry the SUPERSEDING request's origin \
                     (in_flight={in_flight_origin:?}, new={new_origin:?}), not the \
                     aborted transmission's — otherwise the key-time arm gate is \
                     evaluated against the wrong origin"
                );
                // PAN-38 round 4 (Codex): Replace must ALSO carry the
                // superseding request's own qso_id, not the aborted
                // in-flight transmission's — otherwise the caller's
                // in-place retry pairs the new frame's text with the
                // WRONG QSO's id, misattributing its eventual
                // TransmitComplete.
                assert_eq!(
                    qso_id,
                    Some("qso-new".to_string()),
                    "Replace must carry the SUPERSEDING request's own qso_id"
                );
            }
            other => panic!("expected Replace, got {other:?}"),
        }
    }
}

/// C1 CONSISTENCY: a `Bundle` fold must be gated if EITHER the in-flight
/// stream or the superseding request is `Remote` (fail-safe fold), while the
/// separately-returned `new_origin` (used by the caller's frequency-collision
/// fallback, which transmits ONLY the new item) tracks the new request's own
/// origin. Mirrors the coalescer's fail-safe origin fold so a Remote item
/// folded onto a Local in-flight bundle can never re-enter ungated.
#[tokio::test(flavor = "current_thread")]
async fn supersede_bundle_folds_origin_failsafe() {
    use crate::message_bus::TxOrigin;

    // (in_flight, new) -> (expected bundle_origin, expected new_origin)
    let cases = [
        (
            TxOrigin::Local,
            TxOrigin::Remote,
            TxOrigin::Remote,
            TxOrigin::Remote,
        ),
        (
            TxOrigin::Remote,
            TxOrigin::Local,
            TxOrigin::Remote,
            TxOrigin::Local,
        ),
        (
            TxOrigin::Local,
            TxOrigin::Local,
            TxOrigin::Local,
            TxOrigin::Local,
        ),
        (
            TxOrigin::Remote,
            TxOrigin::Remote,
            TxOrigin::Remote,
            TxOrigin::Remote,
        ),
    ];

    for (in_flight_origin, new_origin, want_bundle, want_new) in cases {
        let bus = MessageBus::new(16).unwrap();
        let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

        let now = chrono::Utc::now();
        let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
            pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
            pancetta_core::slot::SLOT_NS,
        );

        let mut message_text = "OLD TEXT".to_string();
        let mut frequency_offset = 1000.0;
        let mut schedule = super::schedule_tx(
            now,
            cur_parity,
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
        );
        let ptt_active = Arc::new(AtomicBool::new(true));
        let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
        let last_ptt_off_ms = Arc::new(AtomicU64::new(0));
        let in_flight = [crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }];

        let new_request = MessageType::TransmitRequest {
            attempt_id: None,
            message_text: "CQ K5ARH EM12".to_string(),
            frequency_offset: 1400.0,
            qso_id: Some("qso-2".to_string()),
            tx_parity: Some(cur_parity),
            origin: new_origin,
            remote_client_key_id: None,
        };

        let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
        let outcome = super::supersede_and_rekey_or_bundle(
            new_request,
            super::WorkingFrame {
                message_text: &mut message_text,
                frequency_offset: &mut frequency_offset,
                schedule: &mut schedule,
            },
            &mut guard,
            &super::SlotTimingContext::ft8_for_test(
                20_000,
                12_000,
                pancetta_core::slot::SLOT_NS,
                pancetta_config::station::TxSelfParity::Auto,
                now,
            ),
            2,
            &super::SupersedeInFlight {
                items: &in_flight,
                origin: in_flight_origin,
                remote_client_key_id: None,
                tx_parity: None,
            },
        )
        .await;

        match outcome {
            super::SupersedeOutcome::Bundle {
                bundle_origin,
                new_origin: got_new,
                ..
            } => {
                assert_eq!(
                    bundle_origin, want_bundle,
                    "bundle_origin fail-safe fold wrong for \
                     in_flight={in_flight_origin:?}, new={new_origin:?}"
                );
                assert_eq!(
                    got_new, want_new,
                    "new_origin must track the superseding request's own origin"
                );
            }
            other => panic!("expected Bundle, got {other:?}"),
        }
    }
}

/// A `MultiTransmitRequest` arriving AS the superseding message isn't
/// re-keyable content — it must report `NotViable` and leave the working
/// text untouched (no schedule mutation).
#[tokio::test(flavor = "current_thread")]
async fn supersede_declines_multi_transmit_request() {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

    let mut message_text = "OLD TEXT".to_string();
    let mut frequency_offset = 1000.0;
    let mut schedule = super::schedule_tx(
        chrono::Utc::now(),
        pancetta_core::slot::SlotParity::Even,
        8000,
        12_000,
        pancetta_core::slot::SLOT_NS,
    );
    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

    let new_request = MessageType::MultiTransmitRequest {
        items: Vec::new(),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };

    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    let outcome = super::supersede_and_rekey_or_bundle(
        new_request,
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
        },
        &mut guard,
        &super::SlotTimingContext::ft8_for_test(
            8000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            chrono::Utc::now(),
        ),
        2,
        &super::SupersedeInFlight {
            items: &[],
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: None,
        },
    )
    .await;

    // PAN-279: the declined bundle comes back inside `NotViable`, so the
    // caller can requeue it.
    assert!(matches!(
        outcome,
        super::SupersedeOutcome::NotViable { ref superseder }
            if matches!(**superseder, MessageType::MultiTransmitRequest { ref items, .. } if items.is_empty())
    ));
    assert_eq!(
        message_text, "OLD TEXT",
        "a declined supersede leaves the working text untouched"
    );
    // Even on the wrong-type NotViable path, PTT must be deasserted (the
    // stuck-PTT bug: PTT-off used to sit AFTER the destructure and never
    // ran here). Since PAN-273 the release goes through the caller's
    // guard, so the multi-TX arm no longer disarms after it.
    assert!(
        !ptt_active.load(Ordering::Acquire),
        "PTT must be deasserted even when the supersede is declined"
    );
}

/// End-to-end regression for the multi-TX arm's supersede integration and
/// the stuck-PTT bug it had: when a `MultiTransmitRequest` supersedes an
/// in-flight multi-TX bundle, `supersede_multi_reenqueue` must (1) actually
/// deassert PTT — both the `ptt_active` flag AND a real `SetPtt{false}`
/// message on the Hamlib channel (a flag-only unkey would leave the rig
/// physically keyed) — and (2) re-enqueue the
/// superseding bundle rather than silently dropping it.
#[tokio::test(flavor = "current_thread")]
async fn multi_reenqueue_deasserts_ptt_and_reenqueues_superseding_bundle() {
    let bus = MessageBus::new(16).unwrap();
    // Hamlib channel receives the PTT-OFF; Ft8Transmitter channel receives
    // the worker's re-enqueue-to-self.
    let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    let (_tx_tx, tx_rx) = bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();

    let mut encoder = super::Ft8Encoder::new();

    // The bundle currently on the air.
    let in_flight = [crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: "KA1ABC K5ARH R-15".to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("qso-1".to_string()),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }];

    // The superseding message is itself a MultiTransmitRequest — the routine
    // concurrent-QSO path the autonomous engine enqueues every slot.
    let superseding = MessageType::MultiTransmitRequest {
        items: vec![
            crate::message_bus::TransmitRequestItem {
                attempt_id: None,
                message_text: "JA1XYZ K5ARH 73".to_string(),
                frequency_offset: 1300.0,
                qso_id: Some("qso-2".to_string()),
                origin: crate::message_bus::TxOrigin::Local,
                remote_client_key_id: None,
            },
            crate::message_bus::TransmitRequestItem {
                attempt_id: None,
                message_text: "CQ K5ARH EM12".to_string(),
                frequency_offset: 1700.0,
                qso_id: None,
                origin: crate::message_bus::TxOrigin::Local,
                remote_client_key_id: None,
            },
        ],
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };

    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

    let inbox = super::TxInbox::new(tx_rx.clone(), Default::default());
    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    super::supersede_multi_reenqueue(
        superseding,
        &super::SupersedeInFlight {
            items: &in_flight,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: None,
        },
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            chrono::Utc::now(),
        ),
        2,
        super::SupersedeIo {
            message_bus: &bus,
            inbox: &inbox,
            encoder: &mut encoder,
            ptt_guard: &mut guard,
        },
    )
    .await;

    // (1a) The in-memory flag is cleared.
    assert!(
        !ptt_active.load(Ordering::Acquire),
        "ptt_active must be cleared"
    );
    // (1b) A real SetPtt{false} was sent to Hamlib — this is the assertion
    // that would have caught the stuck-PTT bug (the flag alone did not).
    let ptt_msg = hamlib_rx
        .try_recv()
        .expect("a PTT-OFF message must be sent to Hamlib");
    assert!(
        matches!(
            ptt_msg.message_type,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state: false })
        ),
        "expected SetPtt{{false}}, got {:?}",
        ptt_msg.message_type
    );

    // (2) The superseding bundle was re-enqueued to the worker, not dropped.
    let reenqueued = inbox
        .try_recv()
        .expect("the superseding MultiTransmitRequest must be re-enqueued");
    match reenqueued.message_type {
        MessageType::MultiTransmitRequest { items, .. } => {
            assert_eq!(items.len(), 2, "the whole superseding bundle is preserved");
            assert_eq!(items[0].message_text, "JA1XYZ K5ARH 73");
            assert_eq!(items[1].message_text, "CQ K5ARH EM12");
        }
        other => panic!("expected re-enqueued MultiTransmitRequest, got {other:?}"),
    }
    // Exactly one thing re-enqueued (no duplicate transmission).
    assert!(
        inbox.try_recv().is_err(),
        "no second re-enqueue — the in-flight bundle is abandoned, not duplicated"
    );
}

/// PR #348 review round 2 (Codex P2): when a `Replace` reaches
/// `supersede_multi_reenqueue`, it can only mean the candidate bundle
/// would have exceeded `max_concurrent_qsos` (a frequency collision
/// instead produces `Bundle` + an encode-failure fallback, never
/// `Replace`, and this function is only ever called from the multi-TX
/// arm, so `in_flight_items` is always a real ≥1-item bundle). The
/// previously-admitted in-flight bundle must be PRESERVED (re-enqueued
/// unchanged), not silently abandoned to make room for the new,
/// over-cap request — which is instead deferred as its own request for
/// the coalescer to retry next cycle.
#[tokio::test(flavor = "current_thread")]
async fn multi_reenqueue_preserves_in_flight_bundle_when_new_request_would_exceed_cap() {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    let (_tx_tx, tx_rx) = bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let (_autonomous_tx, autonomous_rx) =
        bus.create_channel(ComponentId::Autonomous).await.unwrap();

    let mut encoder = super::Ft8Encoder::new();

    // Already a full 2-item bundle in flight — the configured cap.
    let in_flight = [
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "OTHER W5AU R-08".to_string(),
            frequency_offset: 1400.0,
            qso_id: Some("qso-other".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
    ];

    // A THIRD, genuinely new QSO request supersedes.
    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );
    let superseding = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "THIRD W5AU EM10".to_string(),
        frequency_offset: 1800.0,
        qso_id: Some("qso-third".to_string()),
        tx_parity: Some(cur_parity),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };

    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

    let inbox = super::TxInbox::new(tx_rx.clone(), Default::default());
    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    super::supersede_multi_reenqueue(
        superseding,
        &super::SupersedeInFlight {
            items: &in_flight,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: Some(cur_parity),
        },
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
        ),
        2, // max_concurrent_qsos — already met by the 2 in-flight items
        super::SupersedeIo {
            message_bus: &bus,
            inbox: &inbox,
            encoder: &mut encoder,
            ptt_guard: &mut guard,
        },
    )
    .await;

    // The preserved in-flight bundle is re-enqueued first, unchanged.
    let preserved = inbox
        .try_recv()
        .expect("the in-flight bundle must be preserved, not dropped");
    match preserved.message_type {
        MessageType::MultiTransmitRequest {
            items, tx_parity, ..
        } => {
            assert_eq!(items.len(), 2, "both in-flight items must be preserved");
            assert_eq!(items[0].message_text, "KA1ABC K5ARH R-15");
            assert_eq!(items[1].message_text, "OTHER W5AU R-08");
            assert_eq!(
                tx_parity,
                Some(cur_parity),
                "PR #348 review round 3 (Codex P1): the preserved bundle must carry the \
                 ORIGINAL bundle's own tx_parity, not None — losing it would let \
                 resolve_required_parity pick the wrong slot for an explicit-parity bundle \
                 on the next dequeue"
            );
        }
        other => panic!("expected the preserved MultiTransmitRequest, got {other:?}"),
    }

    // PR #348 review round 6 (Codex P1): the over-cap new request must
    // NOT be re-enqueued onto the transmitter channel at all — doing so
    // livelocks (the worker's very next pre-PTT/pre-audio wait would
    // find it sitting right behind the just-preserved bundle and treat
    // it as an immediate fresh supersede, re-entering this same arm
    // forever). Nothing else goes to the transmitter channel.
    assert!(
        inbox.try_recv().is_err(),
        "exactly one message re-enqueued to the transmitter: the preserved bundle — the \
         over-cap request must be dropped, not requeued behind it"
    );

    // Instead, a failed TransmitComplete reports the drop.
    let complete = autonomous_rx
        .try_recv()
        .expect("a failed TransmitComplete must report the dropped over-cap request");
    match complete.message_type {
        MessageType::TransmitComplete {
            success,
            message_text,
            qso_id,
            ..
        } => {
            assert!(!success, "the dropped over-cap request must report failure");
            assert_eq!(message_text, "THIRD W5AU EM10");
            assert_eq!(qso_id.as_deref(), Some("qso-third"));
        }
        other => panic!("expected TransmitComplete, got {other:?}"),
    }
}

/// Codex P1, PR #362 round 18: an in-flight bundle bound to one remote
/// client and a superseding request bound to a DIFFERENT remote client
/// (a legitimate control transfer, not over-capacity) must NOT be
/// treated like the over-cap case above — the new request must be
/// enqueued (not dropped), and the stale in-flight bundle must NOT be
/// re-enqueued (it would only be rejected later against the new arm
/// anyway, and re-enqueuing it would recreate the round-6 livelock).
#[tokio::test(flavor = "current_thread")]
async fn multi_reenqueue_keys_the_new_request_on_identity_conflict_instead_of_preserving_the_stale_bundle(
) {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    let (_tx_tx, tx_rx) = bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let (_autonomous_tx, autonomous_rx) =
        bus.create_channel(ComponentId::Autonomous).await.unwrap();

    let mut encoder = super::Ft8Encoder::new();

    // A 2-item bundle in flight, bound to client-a.
    let in_flight = [
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Remote,
            remote_client_key_id: Some("client-a".to_string()),
        },
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "OTHER W5AU R-08".to_string(),
            frequency_offset: 1400.0,
            qso_id: Some("qso-other".to_string()),
            origin: crate::message_bus::TxOrigin::Remote,
            remote_client_key_id: Some("client-a".to_string()),
        },
    ];

    // Control has transferred to client-b, whose request supersedes.
    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );
    let superseding = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "K5ARH KA1ABC 73".to_string(),
        frequency_offset: 1500.0,
        qso_id: Some("qso-1".to_string()),
        tx_parity: Some(cur_parity),
        origin: crate::message_bus::TxOrigin::Remote,
        remote_client_key_id: Some("client-b".to_string()),
    };

    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

    let inbox = super::TxInbox::new(tx_rx.clone(), Default::default());
    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    super::supersede_multi_reenqueue(
        superseding,
        &super::SupersedeInFlight {
            items: &in_flight,
            origin: crate::message_bus::TxOrigin::Remote,
            remote_client_key_id: Some("client-a".to_string()),
            tx_parity: Some(cur_parity),
        },
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
        ),
        2,
        super::SupersedeIo {
            message_bus: &bus,
            inbox: &inbox,
            encoder: &mut encoder,
            ptt_guard: &mut guard,
        },
    )
    .await;

    // The stale client-a bundle must NOT be re-enqueued as a
    // MultiTransmitRequest — it's abandoned, not preserved.
    // The NEW client-b request must be the only thing sent to the
    // transmitter channel, as a single TransmitRequest carrying its OWN
    // identity.
    let reenqueued = inbox
        .try_recv()
        .expect("the new request must be enqueued, not silently dropped");
    match reenqueued.message_type {
        MessageType::TransmitRequest {
            message_text,
            qso_id,
            origin,
            remote_client_key_id,
            tx_parity,
            ..
        } => {
            assert_eq!(message_text, "K5ARH KA1ABC 73");
            assert_eq!(qso_id.as_deref(), Some("qso-1"));
            assert_eq!(origin, crate::message_bus::TxOrigin::Remote);
            assert_eq!(remote_client_key_id.as_deref(), Some("client-b"));
            assert_eq!(
                tx_parity,
                Some(cur_parity),
                "Codex P1, round 19: the re-enqueued identity-conflict replacement \
                 must carry the NEW request's own latched tx_parity, not None — \
                 otherwise TxSelfParity::Auto recomputes the nearest slot on the \
                 next dequeue, which can pick a sequential (colliding) slot instead"
            );
        }
        other => panic!("expected the new request as a single TransmitRequest, got {other:?}"),
    }
    assert!(
        inbox.try_recv().is_err(),
        "the stale in-flight bundle must not also be re-enqueued"
    );

    // Both abandoned in-flight items roll back with the QSO engine; only
    // the other-QSO item fails to Autonomous (PAN-38 round 5). PAN-279:
    // the requeued request for qso-1 owns qso-1's Autonomous outcome, so
    // its abandoned item sends Autonomous nothing.
    let mut reported: Vec<(bool, String, Option<String>)> = Vec::new();
    while let Ok(msg) = autonomous_rx.try_recv() {
        if let MessageType::TransmitComplete {
            success,
            message_text,
            qso_id,
            ..
        } = msg.message_type
        {
            reported.push((success, message_text, qso_id));
        }
    }
    assert_eq!(
        reported,
        vec![(
            false,
            "OTHER W5AU R-08".to_string(),
            Some("qso-other".to_string())
        )],
        "only the other-QSO abandoned item fails to Autonomous (PAN-279)"
    );
}

// ── PAN-279: every abandoned in-flight item settles through one path ──

/// Requeued messages, Autonomous `(qso, success, attempt)` completions
/// and Qso `(qso, attempt)` not-sent reports from one multi-arm supersede.
type Pan279Reports = (
    Vec<MessageType>,
    Vec<(Option<String>, bool, Option<pancetta_qso::TxAttemptId>)>,
    Vec<(String, Option<pancetta_qso::TxAttemptId>)>,
);

fn pan279_item(
    qso: &str,
    text: &str,
    hz: f64,
    attempt: u64,
    origin: crate::message_bus::TxOrigin,
    key: Option<&str>,
) -> crate::message_bus::TransmitRequestItem {
    crate::message_bus::TransmitRequestItem {
        attempt_id: Some(pancetta_qso::TxAttemptId(attempt)),
        message_text: text.to_string(),
        frequency_offset: hz,
        qso_id: Some(qso.to_string()),
        origin,
        remote_client_key_id: key.map(str::to_string),
    }
}

fn pan279_local(
    qso: &str,
    text: &str,
    hz: f64,
    attempt: u64,
) -> crate::message_bus::TransmitRequestItem {
    pan279_item(
        qso,
        text,
        hz,
        attempt,
        crate::message_bus::TxOrigin::Local,
        None,
    )
}

fn pan279_parities() -> (
    pancetta_core::slot::SlotParity,
    pancetta_core::slot::SlotParity,
) {
    let now = chrono::Utc::now();
    let cur = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );
    (cur, cur.opposite())
}

fn pan279_request(
    item: &crate::message_bus::TransmitRequestItem,
    tx_parity: Option<pancetta_core::slot::SlotParity>,
) -> MessageType {
    MessageType::TransmitRequest {
        attempt_id: item.attempt_id,
        message_text: item.message_text.clone(),
        frequency_offset: item.frequency_offset,
        qso_id: item.qso_id.clone(),
        tx_parity,
        origin: item.origin,
        remote_client_key_id: item.remote_client_key_id.clone(),
    }
}

async fn pan279_multi(
    new_request: MessageType,
    in_flight: &[crate::message_bus::TransmitRequestItem],
    origin: crate::message_bus::TxOrigin,
    key: Option<&str>,
    max_concurrent_qsos: u32,
) -> Pan279Reports {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    let (_tx_tx, tx_rx) = bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let (_, autonomous_rx) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso_rx) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let mut encoder = super::Ft8Encoder::new();
    let inbox = super::TxInbox::new(tx_rx.clone(), Default::default());
    let (cur, _) = pan279_parities();
    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));
    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    super::supersede_multi_reenqueue(
        new_request,
        &super::SupersedeInFlight {
            items: in_flight,
            origin,
            remote_client_key_id: key.map(str::to_string),
            tx_parity: Some(cur),
        },
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            chrono::Utc::now(),
        ),
        max_concurrent_qsos,
        super::SupersedeIo {
            message_bus: &bus,
            inbox: &inbox,
            encoder: &mut encoder,
            ptt_guard: &mut guard,
        },
    )
    .await;
    let requeued = std::iter::from_fn(|| inbox.try_recv().ok())
        .map(|m| m.message_type)
        .collect();
    let autonomous = std::iter::from_fn(|| autonomous_rx.try_recv().ok())
        .filter_map(|m| match m.message_type {
            MessageType::TransmitComplete {
                success,
                qso_id,
                attempt_id,
                ..
            } => Some((qso_id, success, attempt_id)),
            _ => None,
        })
        .collect();
    let mut qso: Vec<_> = std::iter::from_fn(|| qso_rx.try_recv().ok())
        .filter_map(|m| match m.message_type {
            MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent {
                qso_id,
                attempt_id,
                ..
            }) => Some((qso_id, attempt_id)),
            _ => None,
        })
        .collect();
    qso.sort_by_key(|(_, a)| a.map(|a| a.0));
    (requeued, autonomous, qso)
}

fn pan279_not_sent(rows: &[(&str, u64)]) -> Vec<(String, Option<pancetta_qso::TxAttemptId>)> {
    rows.iter()
        .map(|(q, a)| (q.to_string(), Some(pancetta_qso::TxAttemptId(*a))))
        .collect()
}

fn pan279_failed(
    qso: &str,
    attempt: u64,
) -> (Option<String>, bool, Option<pancetta_qso::TxAttemptId>) {
    (
        Some(qso.to_string()),
        false,
        Some(pancetta_qso::TxAttemptId(attempt)),
    )
}

/// PAN-279: an identity-conflict Replace requeues the new request; the
/// abandoned item for its QSO rolls back with the QSO engine only, and
/// the other-QSO item also fails to Autonomous.
#[tokio::test(flavor = "current_thread")]
async fn pan279_identity_conflict_withholds_the_same_qso_item() {
    let remote = crate::message_bus::TxOrigin::Remote;
    let in_flight = [
        pan279_item(
            "qso-1",
            "W1ABC K1AAA R-10",
            1000.0,
            1,
            remote,
            Some("client-a"),
        ),
        pan279_item(
            "qso-2",
            "W1ABC K1BBB R-12",
            1400.0,
            2,
            remote,
            Some("client-a"),
        ),
    ];
    let (cur, _) = pan279_parities();
    let new = pan279_item(
        "qso-1",
        "W1ABC K1AAA RR73",
        1000.0,
        9,
        remote,
        Some("client-b"),
    );
    let (requeued, autonomous, qso) = pan279_multi(
        pan279_request(&new, Some(cur)),
        &in_flight,
        remote,
        Some("client-a"),
        2,
    )
    .await;
    assert_eq!(requeued.len(), 1);
    assert!(matches!(
        &requeued[0],
        MessageType::TransmitRequest {
            attempt_id: Some(pancetta_qso::TxAttemptId(9)),
            ..
        }
    ));
    assert_eq!(qso, pan279_not_sent(&[("qso-1", 1), ("qso-2", 2)]));
    assert_eq!(autonomous, vec![pan279_failed("qso-2", 2)]);
}

/// PAN-279: a Bundle that folds the new qso-1 frame in requeues qso-2's
/// item unchanged and rolls back qso-1's replaced frame (attempt 1),
/// which used to be dropped silently. Autonomous hears nothing.
#[tokio::test(flavor = "current_thread")]
async fn pan279_bundle_success_rolls_back_the_replaced_same_qso_item() {
    let local = crate::message_bus::TxOrigin::Local;
    let in_flight = [
        pan279_local("qso-1", "W1ABC K1AAA R-10", 1000.0, 1),
        pan279_local("qso-2", "W1ABC K1BBB R-12", 1400.0, 2),
    ];
    let (cur, _) = pan279_parities();
    let new = pan279_local("qso-1", "W1ABC K1AAA RR73", 1800.0, 9);
    let (requeued, autonomous, qso) =
        pan279_multi(pan279_request(&new, Some(cur)), &in_flight, local, None, 2).await;
    assert_eq!(requeued.len(), 1);
    let MessageType::MultiTransmitRequest { items, .. } = &requeued[0] else {
        panic!("expected the folded bundle, got {:?}", requeued[0]);
    };
    assert!(
        item_carried_unchanged(&in_flight[1], items),
        "qso-2's item is carried unchanged"
    );
    assert!(items
        .iter()
        .any(|i| i.attempt_id == Some(pancetta_qso::TxAttemptId(9))));
    assert_eq!(qso, pan279_not_sent(&[("qso-1", 1)]));
    assert!(autonomous.is_empty(), "{autonomous:?}");
}

/// PAN-279: on the collision fallback only the new qso-1 request is
/// requeued, so both in-flight items roll back; only qso-2 fails to
/// Autonomous.
#[tokio::test(flavor = "current_thread")]
async fn pan279_bundle_collision_fallback_settles_every_abandoned_item() {
    let local = crate::message_bus::TxOrigin::Local;
    let in_flight = [
        pan279_local("qso-1", "W1ABC K1AAA R-10", 1000.0, 1),
        pan279_local("qso-2", "W1ABC K1BBB R-12", 1400.0, 2),
    ];
    let (cur, _) = pan279_parities();
    let new = pan279_local("qso-1", "W1ABC K1AAA RR73", 1410.0, 9);
    let (requeued, autonomous, qso) =
        pan279_multi(pan279_request(&new, Some(cur)), &in_flight, local, None, 2).await;
    assert_eq!(requeued.len(), 1);
    assert!(matches!(
        &requeued[0],
        MessageType::TransmitRequest {
            attempt_id: Some(pancetta_qso::TxAttemptId(9)),
            ..
        }
    ));
    assert_eq!(qso, pan279_not_sent(&[("qso-1", 1), ("qso-2", 2)]));
    assert_eq!(autonomous, vec![pan279_failed("qso-2", 2)]);
}

/// PAN-279: a NotViable bundle superseder that carries a new frame for
/// the in-flight QSO leaves that QSO's Autonomous outcome to itself; the
/// abandoned frame only rolls back its attempt.
#[tokio::test(flavor = "current_thread")]
async fn pan279_notviable_bundle_superseder_withholds_items_it_replaces() {
    let local = crate::message_bus::TxOrigin::Local;
    let in_flight = [
        pan279_local("qso-cq", "CQ W1ABC FN42", 1000.0, 1),
        pan279_local("qso-x", "K1X W1ABC -05", 1400.0, 2),
    ];
    let superseder = MessageType::MultiTransmitRequest {
        items: vec![
            pan279_local("qso-cq", "CQ W1ABC FN42", 1200.0, 3),
            in_flight[1].clone(),
        ],
        tx_parity: None,
        origin: local,
        remote_client_key_id: None,
    };
    let (requeued, autonomous, qso) =
        pan279_multi(superseder.clone(), &in_flight, local, None, 2).await;
    assert_eq!(requeued.len(), 1);
    assert!(matches!(
        &requeued[0],
        MessageType::MultiTransmitRequest { items, .. } if items.len() == 2
    ));
    assert_eq!(qso, pan279_not_sent(&[("qso-cq", 1)]));
    assert!(autonomous.is_empty(), "{autonomous:?}");
}

/// PAN-279 scenario 1 (multi arm): a too-late `TransmitRequest`
/// superseder is requeued intact instead of being lost. Both in-flight
/// items roll back; only qso-2 fails to Autonomous.
#[tokio::test(flavor = "current_thread")]
async fn pan279_multi_notviable_requeues_a_transmit_request_superseder() {
    let local = crate::message_bus::TxOrigin::Local;
    let in_flight = [
        pan279_local("qso-1", "W1ABC K1AAA R-10", 1000.0, 1),
        pan279_local("qso-2", "W1ABC K1BBB R-12", 1400.0, 2),
    ];
    let (_, opposite) = pan279_parities();
    let new = pan279_local("qso-1", "W1ABC K1AAA RR73", 1000.0, 9);
    let request = pan279_request(&new, Some(opposite));
    let (requeued, autonomous, qso) =
        pan279_multi(request.clone(), &in_flight, local, None, 2).await;
    assert_eq!(requeued.len(), 1, "{requeued:?}");
    assert!(matches!(
        &requeued[0],
        MessageType::TransmitRequest {
            attempt_id: Some(pancetta_qso::TxAttemptId(9)),
            tx_parity: Some(p),
            qso_id: Some(q),
            message_text,
            ..
        } if *p == opposite && q == "qso-1" && message_text == "W1ABC K1AAA RR73"
    ));
    assert_eq!(qso, pan279_not_sent(&[("qso-1", 1), ("qso-2", 2)]));
    assert_eq!(autonomous, vec![pan279_failed("qso-2", 2)]);
}

/// PAN-279 scenario 1 (helper): a deferred `TransmitRequest` superseder
/// comes back inside `NotViable`, field by field, with PTT deasserted
/// and the working text untouched.
#[tokio::test(flavor = "current_thread")]
async fn pan279_notviable_returns_the_deferred_superseder_intact() {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    let mut message_text = "K1ABC N0CALL FN20".to_string();
    let mut frequency_offset = 1000.0;
    let mut schedule = super::schedule_tx(
        chrono::Utc::now(),
        pancetta_core::slot::SlotParity::Even,
        8000,
        12_000,
        pancetta_core::slot::SLOT_NS,
    );
    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));
    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    let (_, opposite) = pan279_parities();
    let new = pan279_item(
        "qso-sv",
        "K1ABC N0CALL -10",
        1300.0,
        12,
        crate::message_bus::TxOrigin::Remote,
        Some("client-a"),
    );
    let request = pan279_request(&new, Some(opposite));
    let outcome = super::supersede_and_rekey_or_bundle(
        request.clone(),
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
        },
        &mut guard,
        &super::SlotTimingContext::ft8_for_test(
            3_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            chrono::Utc::now(),
        ),
        2,
        &super::SupersedeInFlight {
            items: &[],
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: None,
        },
    )
    .await;
    let super::SupersedeOutcome::NotViable { superseder } = outcome else {
        panic!("a deferred superseder must be NotViable");
    };
    assert_eq!(format!("{:?}", *superseder), format!("{request:?}"));
    assert!(!ptt_active.load(Ordering::Acquire));
    assert_eq!(message_text, "K1ABC N0CALL FN20");
    assert_eq!(frequency_offset, 1000.0);
}

/// PR #348 review round 4 (Codex P1): a raised effective cap (e.g. Fox
/// mode) makes `SupersedeOutcome::Bundle` newly reachable through
/// `supersede_multi_reenqueue` for an explicit-parity in-flight bundle.
/// That arm's re-enqueued `MultiTransmitRequest` must carry the
/// established bundle parity forward too — the round-3 fix only covered
/// the sibling `Replace` arm.
#[tokio::test(flavor = "current_thread")]
async fn multi_reenqueue_bundle_success_preserves_bundle_parity() {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    let (_tx_tx, tx_rx) = bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();

    let mut encoder = super::Ft8Encoder::new();

    // A single in-flight QSO — cap=2 leaves room for one more.
    let in_flight = [crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: "KA1ABC K5ARH R-15".to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("qso-1".to_string()),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }];

    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );
    // A genuinely new QSO, well clear in frequency — the Bundle path.
    let superseding = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "SECOND W5AU EM10".to_string(),
        frequency_offset: 1400.0,
        qso_id: Some("qso-2".to_string()),
        tx_parity: Some(cur_parity),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };

    let ptt_active = Arc::new(AtomicBool::new(true));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));

    let inbox = super::TxInbox::new(tx_rx.clone(), Default::default());
    let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
    super::supersede_multi_reenqueue(
        superseding,
        &super::SupersedeInFlight {
            items: &in_flight,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: Some(cur_parity),
        },
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
        ),
        2, // max_concurrent_qsos — room for the 2nd QSO, Bundle is viable
        super::SupersedeIo {
message_bus: &bus,
inbox: &inbox,
encoder: // the ESTABLISHED in-flight bundle's own parity
        &mut encoder,
ptt_guard: &mut guard,
},
    )
    .await;

    let reenqueued = inbox
        .try_recv()
        .expect("the folded bundle must be re-enqueued");
    match reenqueued.message_type {
        MessageType::MultiTransmitRequest {
            items, tx_parity, ..
        } => {
            assert_eq!(items.len(), 2, "in-flight + new item both bundled");
            assert_eq!(
                tx_parity,
                Some(cur_parity),
                "the folded bundle must carry the established parity forward, not None"
            );
        }
        other => panic!("expected a folded MultiTransmitRequest, got {other:?}"),
    }
}

/// Grounding check (brief Step 1/2): the pre-existing multi-TX encode path
/// that bundle-add reuses succeeds when two items are well clear of the
/// ~75 Hz FT8 minimum separation, yielding a 2-item summed waveform.
#[test]
fn bundle_add_succeeds_when_frequencies_are_well_separated() {
    let mut encoder = super::Ft8Encoder::new();
    let tx_params = pancetta_ft8::ProtocolParams::from_protocol(pancetta_ft8::Protocol::Ft8);
    let in_flight = [crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: "KA1ABC K5ARH R-15".to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("qso-1".to_string()),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }];
    let new_item = crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: "CQ K5ARH EM12".to_string(),
        frequency_offset: 1400.0, // 400 Hz away — well clear of the ~75 Hz minimum
        qso_id: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    let bundled: Vec<_> = in_flight
        .iter()
        .cloned()
        .chain(std::iter::once(new_item))
        .collect();
    let outcome = super::encode_and_modulate_multi_tx(
        &mut encoder,
        pancetta_ft8::Protocol::Ft8,
        &tx_params,
        &bundled,
    );
    assert!(
        outcome.samples.is_ok(),
        "expected bundle-add to succeed: {:?}",
        outcome.samples
    );
    assert_eq!(outcome.encoded_items.len(), 2);
}

/// Bundle-add falls back (encode error) when the new item collides on
/// frequency with the in-flight item (inside the ~75 Hz minimum) — this is
/// the collision the caller detects to fall back to a single-item replace.
#[test]
fn bundle_add_falls_back_when_frequencies_collide() {
    let mut encoder = super::Ft8Encoder::new();
    let tx_params = pancetta_ft8::ProtocolParams::from_protocol(pancetta_ft8::Protocol::Ft8);
    let in_flight = [crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: "KA1ABC K5ARH R-15".to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("qso-1".to_string()),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }];
    let new_item = crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: "CQ K5ARH EM12".to_string(),
        frequency_offset: 1010.0, // 10 Hz away — well inside the ~75 Hz minimum, must collide
        qso_id: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    let bundled: Vec<_> = in_flight
        .iter()
        .cloned()
        .chain(std::iter::once(new_item))
        .collect();
    let outcome = super::encode_and_modulate_multi_tx(
        &mut encoder,
        pancetta_ft8::Protocol::Ft8,
        &tx_params,
        &bundled,
    );
    assert!(
        outcome.samples.is_err(),
        "expected the frequency collision to be rejected"
    );
}

/// PAN-76: a fresher frame for one of three in-flight Hounds after Fox
/// mode is turned off. With the supersede ceiling raised to the live
/// Fox-admitted count it folds into the bundle; with the restored cap of
/// 1 the whole bundle collapses to the refreshed Hound (the old bug).
#[tokio::test(flavor = "current_thread")]
async fn pan76_supersede_refresh_of_one_hound_bundles_with_the_other_two() {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );

    let in_flight = [
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "W1AAA K5ARH -10".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("hound-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "W2BBB K5ARH -12".to_string(),
            frequency_offset: 1300.0,
            qso_id: Some("hound-2".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "W3CCC K5ARH -08".to_string(),
            frequency_offset: 1600.0,
            qso_id: Some("hound-3".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
    ];
    let refresh = || MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "W1AAA K5ARH RR73".to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("hound-1".to_string()),
        tx_parity: Some(cur_parity),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };

    let mut outcomes = Vec::new();
    for max_concurrent_qsos in [super::supersede_max_concurrent_qsos(1, 3), 1] {
        let mut message_text = "W1AAA K5ARH -10".to_string();
        let mut frequency_offset = 1000.0;
        let mut schedule = super::schedule_tx(
            now,
            cur_parity,
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
        );
        let ptt_active = Arc::new(AtomicBool::new(true));
        let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
        let last_ptt_off_ms = Arc::new(AtomicU64::new(0));
        let mut guard = keyed_guard(&bus, &ptt_active, &last_ptt_on_ms, &last_ptt_off_ms).await;
        outcomes.push(
            super::supersede_and_rekey_or_bundle(
                refresh(),
                super::WorkingFrame {
                    message_text: &mut message_text,
                    frequency_offset: &mut frequency_offset,
                    schedule: &mut schedule,
                },
                &mut guard,
                &super::SlotTimingContext::ft8_for_test(
                    20_000,
                    12_000,
                    pancetta_core::slot::SLOT_NS,
                    pancetta_config::station::TxSelfParity::Auto,
                    now,
                ),
                max_concurrent_qsos,
                &super::SupersedeInFlight {
                    items: &in_flight,
                    origin: crate::message_bus::TxOrigin::Local,
                    remote_client_key_id: None,
                    tx_parity: None,
                },
            )
            .await,
        );
    }

    match &outcomes[0] {
        super::SupersedeOutcome::Bundle { items, .. } => {
            assert_eq!(
                items.len(),
                3,
                "all three Hounds stay in the bundle: {items:?}"
            );
            let hound1 = items
                .iter()
                .find(|it| it.qso_id.as_deref() == Some("hound-1"))
                .expect("hound-1 must still be present");
            assert_eq!(hound1.message_text, "W1AAA K5ARH RR73");
        }
        other => panic!("expected Bundle under the raised ceiling, got {other:?}"),
    }
    assert!(
        matches!(outcomes[1], super::SupersedeOutcome::Replace { .. }),
        "under the restored cap of 1 the bundle collapses (today's behaviour), got {:?}",
        outcomes[1]
    );
}

/// PR #421 review (Codex P1): an in-flight bundle of two Hounds plus one
/// ordinary QSO (admitted under the restored cap of 1) after Fox mode is
/// turned off. The coalescer kept all three: the Fox-admitted streams on
/// top of the normal cap. A fresher frame for one Hound must fold into
/// that bundle, not trip a ceiling of only the live Fox count (2) and
/// collapse the bundle to the refreshed Hound, keying it stale later.
#[tokio::test(flavor = "current_thread")]
async fn pan76_supersede_refresh_of_a_hound_in_a_mixed_bundle_keeps_every_stream() {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );
    let item = |text: &str, freq: f64, id: &str| crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: freq,
        qso_id: Some(id.to_string()),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    let in_flight = [
        item("W1AAA K5ARH -10", 1000.0, "hound-1"),
        item("W2BBB K5ARH -12", 1300.0, "hound-2"),
        item("W9ZZZ K5ARH R-05", 1600.0, "ordinary"),
    ];

    let mut message_text = "W1AAA K5ARH -10".to_string();
    let mut frequency_offset = 1000.0;
    let mut schedule = super::schedule_tx(
        now,
        cur_parity,
        20_000,
        12_000,
        pancetta_core::slot::SLOT_NS,
    );
    let mut guard = keyed_guard(
        &bus,
        &Arc::new(AtomicBool::new(true)),
        &Arc::new(AtomicU64::new(0)),
        &Arc::new(AtomicU64::new(0)),
    )
    .await;
    let outcome = super::supersede_and_rekey_or_bundle(
        MessageType::TransmitRequest {
            attempt_id: None,
            message_text: "W1AAA K5ARH RR73".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("hound-1".to_string()),
            tx_parity: Some(cur_parity),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
        },
        &mut guard,
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
        ),
        // Fox off: normal cap 1, two Hounds still live.
        super::supersede_max_concurrent_qsos(1, 2),
        &super::SupersedeInFlight {
            items: &in_flight,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: None,
        },
    )
    .await;

    match &outcome {
        super::SupersedeOutcome::Bundle { items, .. } => {
            assert_eq!(items.len(), 3, "every in-flight stream stays: {items:?}");
            let hound1 = items
                .iter()
                .find(|it| it.qso_id.as_deref() == Some("hound-1"))
                .expect("hound-1 must still be present");
            assert_eq!(hound1.message_text, "W1AAA K5ARH RR73");
        }
        other => panic!("expected the refresh to bundle, got {other:?}"),
    }
}
