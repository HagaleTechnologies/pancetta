//! PAN-385 AC17: the hop executor waits while keyed or mid-exchange, and
//! otherwise tears down, retunes and says why.

use super::*;
use crossbeam_channel::Receiver;
use pancetta_core::DiagnosticLevel;
use std::time::Duration;

const FROM_HZ: u64 = 14_074_000;
const TO_HZ: u64 = 18_100_000;
const AC1_TEXT: &str = "Band hop 20m -> 17m: needed activity; 2 needed entities spotted on 17m, \
                        0 on 20m; no needed caller heard on 20m for 10 min";

struct Harness {
    ctx: BandHopCtx,
    qso_rx: Receiver<ComponentMessage>,
    hamlib_rx: Receiver<ComponentMessage>,
    tui_rx: Receiver<ComponentMessage>,
}

async fn harness(ptt_active: bool, active_is_ft4: bool) -> Harness {
    let bus = MessageBus::new(64).unwrap();
    let (_, qso_rx) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let (_, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    let (_, tui_rx) = bus.create_channel(ComponentId::Tui).await.unwrap();
    Harness {
        ctx: BandHopCtx {
            bus,
            operating_frequency_hz: Arc::new(AtomicU64::new(FROM_HZ)),
            last_freq_command: Arc::new(std::sync::Mutex::new(None)),
            split_tx_hz: Arc::new(AtomicU64::new(0)),
            display_feed_enabled: Arc::new(AtomicBool::new(false)),
            ptt_active: Arc::new(AtomicBool::new(ptt_active)),
            ptt_sync_gate: Arc::new(tokio::sync::Mutex::new(())),
            active_is_ft4,
        },
        qso_rx,
        hamlib_rx,
        tui_rx,
    }
}

/// The AC1 needed-activity reason, 20m -> 17m.
fn ac1_reason() -> pancetta_qso::BandHopReason {
    pancetta_qso::BandHopReason::NeededActivity {
        from: Some(Band::Band20m),
        to: Band::Band17m,
        to_needed: 2,
        to_atno: 0,
        from_needed: 0,
        idle_mins: 10,
        grey_line: None,
    }
}

fn drain(rx: &Receiver<ComponentMessage>) -> Vec<ComponentMessage> {
    rx.try_iter().collect()
}

fn set_freqs(msgs: &[ComponentMessage]) -> Vec<u64> {
    msgs.iter()
        .filter_map(|m| match m.message_type {
            MessageType::RigControl(RigControlMessage::SetFrequency { vfo: 0, frequency }) => {
                Some(frequency)
            }
            _ => None,
        })
        .collect()
}

fn assert_sent_nothing(h: &Harness) {
    assert!(drain(&h.qso_rx).is_empty(), "no BandChanged");
    assert!(drain(&h.hamlib_rx).is_empty(), "no SetFrequency");
    assert!(drain(&h.tui_rx).is_empty(), "no Tui message");
    assert_eq!(
        h.ctx.operating_frequency_hz.load(Ordering::Relaxed),
        FROM_HZ
    );
    assert!(h.ctx.last_freq_command.lock().unwrap().is_none());
}

#[tokio::test]
async fn unkeyed_idle_hop_tears_down_then_retunes_and_reports_why() {
    let h = harness(false, false).await;
    let reason = ac1_reason();
    assert_eq!(reason.why_text(), AC1_TEXT);

    let outcome = execute_band_hop(&h.ctx, TO_HZ, &reason, &[]).await;

    assert_eq!(
        outcome,
        BandHopOutcome::Moved {
            previous_hz: FROM_HZ,
            dial_hz: TO_HZ
        }
    );
    let qso = drain(&h.qso_rx);
    assert_eq!(qso.len(), 1, "qso={qso:?}");
    let teardown = &qso[0];
    assert!(matches!(
        teardown.message_type,
        MessageType::QsoMessage(QsoMessage::BandChanged {
            previous_hz: FROM_HZ,
            new_hz: TO_HZ
        })
    ));
    let hamlib = drain(&h.hamlib_rx);
    assert_eq!(set_freqs(&hamlib), vec![TO_HZ]);
    // The teardown goes out before the retune.
    assert!(
        teardown.id < hamlib[0].id,
        "BandChanged must precede SetFrequency"
    );
    assert_eq!(h.ctx.operating_frequency_hz.load(Ordering::Relaxed), TO_HZ);
    let anchor = h.ctx.last_freq_command.lock().unwrap().expect("C9 anchor");
    assert_eq!(anchor.0, TO_HZ);
    let diagnostics: Vec<_> = drain(&h.tui_rx)
        .into_iter()
        .filter_map(|m| match m.message_type {
            MessageType::DiagnosticEvent {
                target,
                level,
                text,
                qso_id,
                callsign,
            } => Some((target, level, text, qso_id, callsign)),
            _ => None,
        })
        .collect();
    assert_eq!(
        diagnostics,
        vec![(
            "qso.autonomous",
            DiagnosticLevel::Info,
            AC1_TEXT.to_string(),
            None,
            None
        )]
    );
}

#[tokio::test]
async fn keyed_rig_waits_and_sends_nothing() {
    let h = harness(true, false).await;

    let outcome = execute_band_hop(&h.ctx, TO_HZ, &ac1_reason(), &[]).await;

    assert_eq!(
        outcome,
        BandHopOutcome::Waiting("Band hop 20m -> 17m waiting: rig is keyed".into())
    );
    assert_sent_nothing(&h);

    // A key-down mid-enqueue (manual PTT / tune) counts as keyed too.
    let h = harness(false, false).await;
    h.ctx.bus.gain_tx_gate.store(2, Ordering::Release);
    let outcome = execute_band_hop(&h.ctx, TO_HZ, &ac1_reason(), &[]).await;
    assert_eq!(
        outcome,
        BandHopOutcome::Waiting("Band hop 20m -> 17m waiting: rig is keyed".into())
    );
    assert_sent_nothing(&h);
}

#[tokio::test]
async fn exchange_in_progress_waits_and_sends_nothing() {
    let h = harness(false, false).await;

    let outcome = execute_band_hop(&h.ctx, TO_HZ, &ac1_reason(), &["K1ABC".to_string()]).await;
    assert_eq!(
        outcome,
        BandHopOutcome::Waiting("Band hop 20m -> 17m waiting: QSO with K1ABC in progress".into())
    );
    assert_sent_nothing(&h);

    let outcome = execute_band_hop(
        &h.ctx,
        TO_HZ,
        &ac1_reason(),
        &["K1ABC".to_string(), "DL1XYZ".to_string()],
    )
    .await;
    match outcome {
        BandHopOutcome::Waiting(text) => {
            assert!(text.ends_with("2 QSOs in progress"), "{text}");
            assert_eq!(text, "Band hop 20m -> 17m waiting: 2 QSOs in progress");
        }
        other => panic!("expected Waiting, got {other:?}"),
    }
    assert_sent_nothing(&h);
}

#[tokio::test]
async fn held_ptt_sync_gate_waits_without_blocking() {
    // A held gate is a key-down under way: the hop returns at once, sends
    // nothing, and the next slot re-reads the exchange snapshot instead of
    // retuning on this one after the gate opens (a partner may answer while
    // the gate is held).
    let h = harness(false, false).await;
    let held = h.ctx.ptt_sync_gate.clone().lock_owned().await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        execute_band_hop(&h.ctx, TO_HZ, &ac1_reason(), &[]),
    )
    .await
    .expect("the hop never waits on ptt_sync_gate");
    assert_eq!(
        outcome,
        BandHopOutcome::Waiting("Band hop 20m -> 17m waiting: rig is keyed".into())
    );
    assert_sent_nothing(&h);

    // A partner answered while the gate was held: the retry sees it.
    drop(held);
    let outcome = execute_band_hop(&h.ctx, TO_HZ, &ac1_reason(), &["K1ABC".to_string()]).await;
    assert_eq!(
        outcome,
        BandHopOutcome::Waiting("Band hop 20m -> 17m waiting: QSO with K1ABC in progress".into())
    );
    assert_sent_nothing(&h);

    // With the gate free and no exchange, the hop runs.
    assert!(matches!(
        execute_band_hop(&h.ctx, TO_HZ, &ac1_reason(), &[]).await,
        BandHopOutcome::Moved { dial_hz: TO_HZ, .. }
    ));
    assert_eq!(set_freqs(&drain(&h.hamlib_rx)), vec![TO_HZ]);
}

#[tokio::test]
async fn undelivered_set_frequency_restores_the_dial_and_waits() {
    let h = harness(false, false).await;
    // Saturate the registered Hamlib route.
    let mut filled = 0;
    loop {
        let msg = ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Hamlib,
            MessageType::RigControl(RigControlMessage::SetFrequency {
                vfo: 0,
                frequency: FROM_HZ,
            }),
            Instant::now(),
        );
        if !h.ctx.bus.send_message_checked(msg).await.unwrap() {
            break;
        }
        filled += 1;
        assert!(filled < 100_000, "the Hamlib channel never filled");
    }
    let outcome = execute_band_hop(&h.ctx, TO_HZ, &ac1_reason(), &[]).await;
    assert_eq!(
        outcome,
        BandHopOutcome::Waiting(
            "Band hop 20m -> 17m waiting: rig command was not delivered".into()
        )
    );
    assert_eq!(
        h.ctx.operating_frequency_hz.load(Ordering::Relaxed),
        FROM_HZ,
        "the dial is restored"
    );
    assert!(
        h.ctx.last_freq_command.lock().unwrap().is_none(),
        "the C9 anchor is restored"
    );
    assert!(
        drain(&h.tui_rx)
            .iter()
            .all(|m| !matches!(m.message_type, MessageType::DiagnosticEvent { .. })),
        "no Why line for a hop that did not happen"
    );
    let queued = drain(&h.hamlib_rx);
    assert_eq!(queued.len(), filled);
    assert!(set_freqs(&queued).iter().all(|f| *f == FROM_HZ));
}

#[test]
fn exchange_waiting_text_names_the_partners() {
    assert_eq!(exchange_waiting_text(&ac1_reason(), &[]), None);
    assert_eq!(
        exchange_waiting_text(&ac1_reason(), &["K1ABC".to_string()]).as_deref(),
        Some("Band hop 20m -> 17m waiting: QSO with K1ABC in progress")
    );
    assert_eq!(
        exchange_waiting_text(&ac1_reason(), &["K1ABC".to_string(), "DL1XYZ".to_string()])
            .as_deref(),
        Some("Band hop 20m -> 17m waiting: 2 QSOs in progress")
    );
}

#[tokio::test]
async fn ft4_mode_hops_to_the_ft4_dial() {
    let h = harness(false, true).await;

    let outcome = execute_band_hop(&h.ctx, TO_HZ, &ac1_reason(), &[]).await;

    assert_eq!(
        outcome,
        BandHopOutcome::Moved {
            previous_hz: FROM_HZ,
            dial_hz: 18_104_000
        }
    );
    assert_eq!(set_freqs(&drain(&h.hamlib_rx)), vec![18_104_000]);
    assert_eq!(
        h.ctx.operating_frequency_hz.load(Ordering::Relaxed),
        18_104_000
    );
}
