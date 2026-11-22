//! PAN-275: the completed-QSO TX grace is per registration — a later
//! `keep_live` for the same QSO outlives an earlier registration's purge.
use super::CompletedTxGrace;
use std::time::Duration;

const GRACE: Duration = Duration::from_millis(150);

fn grace() -> CompletedTxGrace {
    let mgr = pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    CompletedTxGrace::for_test(&mgr, GRACE)
}

fn live(g: &CompletedTxGrace, key: &str) -> bool {
    g.active_tx_qsos.read().unwrap().contains(key)
}

fn intent() -> crate::coordinator::LatestTxIntent {
    crate::coordinator::LatestTxIntent {
        message_text: "DL1ABC K5ARH 73".to_string(),
        frequency_offset: 1500.0,
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
        attempt_id: None,
        request_queued: true,
    }
}

#[tokio::test]
async fn completed_tx_grace_expires_after_the_grace() {
    let g = grace();
    let id = uuid::Uuid::new_v4();
    let key = crate::coordinator::active_tx_qso_key(&id.to_string());

    g.keep_live(id, 1500.0);
    assert!(live(&g, &key));
    assert_eq!(
        g.active_tx_offsets.read().unwrap().get(&key).copied(),
        Some(1500.0)
    );
    g.latest_tx_intent
        .write()
        .unwrap()
        .insert(key.clone(), intent());
    g.fox_admitted_qsos.write().unwrap().insert(key.clone());

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!live(&g, &key), "purged after the grace");
    assert!(!g.active_tx_offsets.read().unwrap().contains_key(&key));
    assert!(!g.latest_tx_intent.read().unwrap().contains_key(&key));
    assert!(!g.fox_admitted_qsos.read().unwrap().contains(&key));
}

#[tokio::test]
async fn completed_tx_grace_later_registration_survives_an_earlier_expiry() {
    let g = grace();
    let id = uuid::Uuid::new_v4();
    let key = crate::coordinator::active_tx_qso_key(&id.to_string());

    let first = g.keep_live(id, 1500.0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let second = g.keep_live(id, 1500.0);
    assert_ne!(first, second);
    assert!(
        !g.purge_if_current(&key, first),
        "a superseded registration's purge must not remove the key"
    );
    assert!(live(&g, &key));

    // At ~200 ms the first registration's purge has fired; the second
    // registration (live until ~250 ms) still holds the key.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        live(&g, &key),
        "a later registration outlives an earlier purge"
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!live(&g, &key), "purged once the newest grace elapsed");
    assert!(!g.active_tx_offsets.read().unwrap().contains_key(&key));
}
