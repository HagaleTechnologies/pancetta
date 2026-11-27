use super::*;

#[tokio::test]
async fn engine_stamps_decodes_with_the_virtual_slot_time() {
    let mut sim = Sim::new("K5ARH", Some("EM12")).await;
    sim.inject_decode("CQ NP4VA FK68", 1500.0, -10.0, 0.1);
    sim.call_station("NP4VA", 1500.0).await;
    let slot_zero = sim.clock.now();
    sim.tick().await;

    sim.inject_decode("K5ARH NP4VA -13", 1500.0, -10.0, 0.1);
    let slot_one = sim.clock.now();
    sim.tick().await;

    let id = sim
        .manager()
        .get_active_qsos()
        .await
        .into_iter()
        .find(|(_, q)| q.metadata.their_callsign.as_deref() == Some("NP4VA"))
        .expect("NP4VA QSO")
        .0;
    let progress = sim.manager().get_qso(id).await.unwrap();
    assert_eq!(progress.metadata.first_call_at, Some(slot_zero));
    assert_eq!(progress.metadata.last_call_at, Some(slot_one));
    assert!(
        matches!(progress.state, QsoState::SendingReport { started_at, .. } if started_at == slot_one)
    );
}
