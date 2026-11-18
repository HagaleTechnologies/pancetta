use super::*;

fn tune(hz: f64) -> ComponentMessage {
    self_message(MessageType::TuneRequest {
        duration_secs: 5,
        tone_offset_hz: hz,
    })
}
fn hz(m: ComponentMessage) -> f64 {
    match m.message_type {
        MessageType::TuneRequest { tone_offset_hz, .. } => tone_offset_hz,
        other => panic!("expected TuneRequest, got {other:?}"),
    }
}

/// The front is read before the channel and keeps the order it was given.
#[test]
fn requeue_front_is_read_before_the_channel_in_order() {
    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(tune(3000.0)).unwrap();
    let inbox = TxInbox::new(rx, TxRequeue::default());
    inbox.requeue_front(vec![tune(1000.0), tune(2000.0)]);
    inbox.requeue_front(vec![tune(500.0)]);
    assert_eq!(inbox.len(), 4);
    let got: Vec<f64> = std::iter::from_fn(|| inbox.try_recv().ok())
        .map(hz)
        .collect();
    assert_eq!(got, vec![500.0, 1000.0, 2000.0, 3000.0]);
}

/// AC2 for the structure itself: nothing is dropped past the warning mark.
#[test]
fn requeue_front_keeps_every_message_past_the_high_water_mark() {
    let (_tx, rx) = crossbeam_channel::unbounded();
    let inbox = TxInbox::new(rx, TxRequeue::default());
    let n = 2 * TX_REQUEUE_HIGH_WATER + 1;
    inbox.requeue_front((0..n).map(|i| tune(i as f64)).collect());
    assert_eq!(inbox.len(), n);
    let got: Vec<f64> = std::iter::from_fn(|| inbox.try_recv().ok())
        .map(hz)
        .collect();
    assert_eq!(got, (0..n).map(|i| i as f64).collect::<Vec<_>>());
}

/// The worker's `Disconnected => break` must not strand held messages.
#[test]
fn held_messages_come_out_before_a_disconnected_channel_reports_it() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let inbox = TxInbox::new(rx, TxRequeue::default());
    inbox.requeue_front(vec![tune(1000.0)]);
    drop(tx);
    assert!(matches!(inbox.try_recv().map(hz), Ok(v) if v == 1000.0));
    assert!(matches!(
        inbox.try_recv(),
        Err(crossbeam_channel::TryRecvError::Disconnected)
    ));
}

#[test]
fn a_poisoned_requeue_lock_still_yields_its_messages() {
    let (_tx, rx) = crossbeam_channel::unbounded();
    let requeue = TxRequeue::default();
    let inbox = TxInbox::new(rx, requeue.clone());
    inbox.requeue_front(vec![tune(1000.0)]);
    let poisoner = requeue.clone();
    let _ = std::thread::spawn(move || {
        let _held = poisoner.0.lock().unwrap();
        panic!("poison the requeue lock");
    })
    .join();
    assert!(requeue.0.is_poisoned());
    assert!(matches!(inbox.try_recv().map(hz), Ok(v) if v == 1000.0));
}
