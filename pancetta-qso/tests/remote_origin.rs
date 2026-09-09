//! B1 (P3.4c) — a `remote_origin` QSO's emitted `MessageToSend` carries
//! `remote_origin = true`; a normal QSO carries `false`.
//!
//! The coordinator forwards `MessageToSend.remote_origin` into
//! `TransmitRequest.origin` (`Remote` vs `Local`), so this is the engine-level
//! proof that the flag flows through the reply ladder — the security-critical
//! property that a remote-initiated QSO's TX is armed-TX gated end to end.

use pancetta_core::slot::SlotParity;
use pancetta_qso::{CallInitiation, QsoEvent, QsoManager, QsoManagerConfig};

fn config() -> QsoManagerConfig {
    QsoManagerConfig {
        our_callsign: "W1ABC".to_string(),
        our_grid: Some("FN42".to_string()),
        ..QsoManagerConfig::default()
    }
}

/// Drain the manager's event stream and return the `remote_origin` flag of the
/// first `MessageToSend` seen.
async fn first_message_to_send_remote_origin(
    rx: &mut tokio::sync::broadcast::Receiver<QsoEvent>,
) -> bool {
    first_message_to_send_remote_origin_and_client(rx).await.0
}

/// Like [`first_message_to_send_remote_origin`], but also returns
/// `remote_client_key_id` (PAN-91: the QSO must carry the requesting peer's
/// identity, not just a bare boolean).
async fn first_message_to_send_remote_origin_and_client(
    rx: &mut tokio::sync::broadcast::Receiver<QsoEvent>,
) -> (bool, Option<String>) {
    loop {
        match rx.recv().await.expect("event stream closed") {
            QsoEvent::MessageToSend {
                remote_origin,
                remote_client_key_id,
                ..
            } => return (remote_origin, remote_client_key_id),
            _ => continue,
        }
    }
}

#[tokio::test]
async fn remote_origin_qso_emits_remote_message_to_send() {
    let manager = QsoManager::new(config());
    let mut rx = manager.subscribe();

    // A QSO opened with remote_origin=true, bound to a specific client.
    manager
        .respond_to_cq_with(
            "K9XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true, // remote_origin
            Some("client-a".to_string()),
        )
        .await
        .expect("respond_to_cq_with");

    let (origin, client_key_id) = first_message_to_send_remote_origin_and_client(&mut rx).await;
    assert!(
        origin,
        "a remote_origin QSO's MessageToSend MUST carry remote_origin=true \
         (else its TransmitRequest would be Local and bypass the arm)"
    );
    assert_eq!(
        client_key_id.as_deref(),
        Some("client-a"),
        "PAN-91: the QSO must carry the requesting peer's identity so the \
         arm gate can bind TX to THAT client, not just any armed client"
    );
}

#[tokio::test]
async fn normal_qso_emits_local_message_to_send_regression() {
    let manager = QsoManager::new(config());
    let mut rx = manager.subscribe();

    // A normal (local) QSO — every existing path passes remote_origin=false.
    manager
        .respond_to_cq_with(
            "K9XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            false, // remote_origin
            None,
        )
        .await
        .expect("respond_to_cq_with");

    let origin = first_message_to_send_remote_origin(&mut rx).await;
    assert!(
        !origin,
        "a normal QSO's MessageToSend MUST carry remote_origin=false (regression: \
         local TX stays TxOrigin::Local, byte-identical)"
    );
}

/// Round-3 review (Codex P2): when client A creates a manual QSO and client
/// B later takes control and repeats the same accepted action (here:
/// `respond_to_cq_with` for the same callsign/band — the idempotent
/// keep-call path), the EXISTING QSO's bound identity must rebind to B, not
/// stay latched to A. Before this fix `resend_last_tx` re-emitted the QSO's
/// `MessageToSend` with A's stale `remote_client_key_id`, so B's own valid
/// arm would reject every resend and B could never recover control of an
/// in-progress QSO short of it terminating on its own.
#[tokio::test]
async fn repeated_manual_call_rebinds_the_existing_qso_to_the_new_controller() {
    let manager = QsoManager::new(config());
    let mut rx = manager.subscribe();

    let id_a = manager
        .respond_to_cq_with(
            "K9XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true, // remote_origin
            Some("client-a".to_string()),
        )
        .await
        .expect("respond_to_cq_with (client-a)");

    // Drain client-a's opening MessageToSend.
    let (_origin, client_key_id) = first_message_to_send_remote_origin_and_client(&mut rx).await;
    assert_eq!(client_key_id.as_deref(), Some("client-a"));

    // Client B takes control and repeats the same accepted action for the
    // SAME callsign/band — this must resolve to the SAME QSO (never spawn a
    // sibling) via the idempotent keep-call path, and that resend must now
    // carry B's identity.
    let id_b = manager
        .respond_to_cq_with(
            "K9XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true,
            Some("client-b".to_string()),
        )
        .await
        .expect("respond_to_cq_with (client-b)");
    assert_eq!(
        id_a, id_b,
        "a repeated manual call for the same callsign/band must resolve to \
         the same QSO, never spawn a sibling"
    );

    let (_origin, client_key_id) = first_message_to_send_remote_origin_and_client(&mut rx).await;
    assert_eq!(
        client_key_id.as_deref(),
        Some("client-b"),
        "the existing QSO must rebind to the new controller's identity \
         before resending, not stay latched to whoever created it"
    );
}

/// Round-10 review (Codex P2): if client B repeats an accepted action for an
/// A-bound QSO before B's own arm is actually in effect, the rebind must
/// not charge that doomed resend to the manual-call budget — otherwise a
/// handful of denied repeats (each rejected downstream at the TX layer)
/// could exhaust `manual_call_max_calls` before B ever obtains a valid arm,
/// permanently stalling the QSO for B too.
#[tokio::test]
async fn denied_rebound_resend_does_not_charge_the_call_budget() {
    let mut manager = QsoManager::new(config());
    // Nobody is ever TX-permitted in this test — simulates B's repeat
    // landing before B's own arm takes effect.
    manager.set_remote_tx_permitted_source(std::sync::Arc::new(|_: Option<&str>| false));
    let mut rx = manager.subscribe();

    let id_a = manager
        .respond_to_cq_with(
            "K9XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true,
            Some("client-a".to_string()),
        )
        .await
        .expect("respond_to_cq_with (client-a)");
    let _ = first_message_to_send_remote_origin_and_client(&mut rx).await;

    let call_count_before = manager
        .get_qso(id_a)
        .await
        .expect("qso must exist")
        .metadata
        .call_count;

    let id_b = manager
        .respond_to_cq_with(
            "K9XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true,
            Some("client-b".to_string()),
        )
        .await
        .expect("respond_to_cq_with (client-b)");
    assert_eq!(id_a, id_b);

    // No MessageToSend for the denied resend — draining with a short
    // timeout should find nothing.
    let drained = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
    assert!(
        drained.is_err(),
        "a resend the TX layer would deny anyway must not even be attempted"
    );

    let call_count_after = manager
        .get_qso(id_a)
        .await
        .expect("qso must exist")
        .metadata
        .call_count;
    assert_eq!(
        call_count_before, call_count_after,
        "a denied rebound resend must not charge the manual-call budget"
    );
}

/// Round-13 review (Codex P1): unlike the resend branches, advancing an
/// existing QSO to an AHEAD step (`respond_to_caller`'s ladder-advance
/// branch) MUTATES the exchange — records a `Sent` message, changes state,
/// and at `SeventyThree` completes the QSO and logs the ADIF contact. Doing
/// that unconditionally for a client the TX layer would deny is worse than
/// the resend branches' wasted budget charge: it can permanently corrupt
/// the exchange state or produce a false completed-QSO log entry for a
/// contact that never actually transmitted.
#[tokio::test]
async fn denied_rebound_advance_does_not_complete_or_mutate_the_qso() {
    let mut manager = QsoManager::new(config());
    manager.set_remote_tx_permitted_source(std::sync::Arc::new(|_: Option<&str>| false));
    let mut rx = manager.subscribe();

    // Client A opens at Report (SendingReport state) — creates a fresh QSO
    // since none exists yet for this callsign/band.
    let id_a = manager
        .respond_to_caller(
            "K9XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            pancetta_core::ResponseStep::Report,
            None,
            None,
            None,
            true,
            Some("client-a".to_string()),
        )
        .await
        .expect("respond_to_caller (client-a, Report)");
    let _ = first_message_to_send_remote_origin_and_client(&mut rx).await;

    // Client B repeats at SeventyThree — ranked AHEAD of SendingReport, so
    // this hits the ladder-advance branch, not the idempotent resend one.
    // B is not TX-permitted (set above), so this must be denied downstream.
    let id_b = manager
        .respond_to_caller(
            "K9XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            pancetta_core::ResponseStep::SeventyThree,
            None,
            None,
            None,
            true,
            Some("client-b".to_string()),
        )
        .await
        .expect("respond_to_caller (client-b, SeventyThree)");
    assert_eq!(
        id_a, id_b,
        "must resolve to the same QSO, never spawn a sibling"
    );

    // No MessageToSend for the denied advance.
    let drained = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
    assert!(
        drained.is_err(),
        "an advance the TX layer would deny anyway must not even be attempted"
    );

    let progress = manager.get_qso(id_a).await.expect("qso must exist");
    assert!(
        !matches!(progress.state, pancetta_qso::QsoState::Completed { .. }),
        "a denied advance must never complete the QSO (would log a false ADIF contact)"
    );
}

#[tokio::test]
async fn remote_origin_persists_across_the_reply_ladder() {
    // The flag is latched in QsoMetadata at open, so EVERY subsequent
    // MessageToSend for the QSO (keep-calls, auto-sequenced replies) carries it.
    let manager = QsoManager::new(config());
    let mut rx = manager.subscribe();

    let _id = manager
        .start_cq(
            1500.0,
            Some(SlotParity::Odd),
            true,
            Some("client-b".to_string()),
        )
        .await
        .expect("start_cq");

    // Opening CQ MessageToSend is remote, bound to the requesting client.
    let (origin, client_key_id) = first_message_to_send_remote_origin_and_client(&mut rx).await;
    assert!(
        origin,
        "opening CQ of a remote QSO must be remote_origin=true"
    );
    assert_eq!(
        client_key_id.as_deref(),
        Some("client-b"),
        "PAN-91: the manual-CQ remote_origin path must also bind client identity"
    );
}
