//! Codex P1, PR #362 round 17: an auto-reply's origin/client-id used to
//! be captured from the `qsos` map before releasing the write lock, so a
//! takeover that rebinds the QSO's remote identity in the window before
//! the reply is actually emitted got silently overridden by that stale
//! snapshot. `current_remote_binding` re-reads live state at emission
//! time instead — these tests cover its two contracts directly (live
//! value wins; a vanished QSO falls back to the snapshot) rather than
//! trying to race the real async window, which isn't reproducible
//! deterministically.
use super::{
    default_active_mode, AutoSequenceConfig, DuplicateCheckConfig, HoundRegions, QsoId, QsoManager,
    QsoManagerConfig, TimeoutConfig,
};
use pancetta_core::slot::SlotParity;

fn test_config() -> QsoManagerConfig {
    QsoManagerConfig {
        our_callsign: "W1ABC".to_string(),
        our_grid: Some("FN42".to_string()),
        timeouts: TimeoutConfig::default(),
        contest_mode: None,
        contest_scoring: None,
        auto_sequence: AutoSequenceConfig::default(),
        duplicate_checking: DuplicateCheckConfig::default(),
        hound: HoundRegions::default(),
        active_mode: default_active_mode(),
    }
}

#[tokio::test]
async fn live_binding_overrides_a_stale_snapshot() {
    let manager = QsoManager::new(test_config());
    let qso_id = manager
        .respond_to_cq("K9XYZ".to_string(), 14074000.0, Some(SlotParity::Even))
        .await
        .unwrap();

    // A locally-originated QSO starts with no remote binding at all.
    let stale_snapshot = (false, None);
    assert_eq!(
        manager
            .current_remote_binding(qso_id, stale_snapshot.clone())
            .await,
        (false, None)
    );

    // A concurrent takeover rebinds it to a new remote controller —
    // simulating exactly the race the round-17 finding described.
    if let Some(progress) = manager.qsos.write().await.get_mut(&qso_id) {
        progress.metadata.remote_origin = true;
        progress.metadata.remote_client_key_id = Some("newclient".to_string());
    }

    // Reading with the OLD (pre-rebind) snapshot must still return the
    // LIVE binding, not the stale value passed in.
    assert_eq!(
        manager.current_remote_binding(qso_id, stale_snapshot).await,
        (true, Some("newclient".to_string()))
    );
}

#[tokio::test]
async fn vanished_qso_falls_back_to_the_snapshot() {
    let manager = QsoManager::new(test_config());
    let bogus_id = QsoId::new_v4();
    let snapshot = (true, Some("fallback-client".to_string()));

    assert_eq!(
        manager
            .current_remote_binding(bogus_id, snapshot.clone())
            .await,
        snapshot
    );
}
