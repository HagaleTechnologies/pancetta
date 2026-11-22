//! PAN-280: a completed QSO's TX liveness starts on the `Completed` state
//! change (`CompletedTxGrace::keep_live`), and a key that reopens or
//! fails stands its pending grace purge down (`stand_down`).
use super::CompletedTxGrace;
use std::time::Duration;

const GRACE: Duration = Duration::from_millis(60);

fn grace() -> CompletedTxGrace {
    let mgr = pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        ..Default::default()
    });
    CompletedTxGrace::for_test(&mgr, GRACE)
}

fn live(g: &CompletedTxGrace, key: &str) -> bool {
    g.active_tx_qsos.read().unwrap().contains(key)
        && g.active_tx_offsets.read().unwrap().contains_key(key)
}

fn key_of(id: pancetta_qso::QsoId) -> String {
    crate::coordinator::active_tx_qso_key(&id.to_string())
}

#[tokio::test]
async fn completed_state_change_keeps_the_73_live_then_purges_after_grace() {
    let g = grace();
    let id = pancetta_qso::QsoId::new_v4();
    let key = key_of(id);
    g.keep_live(id, 1500.0);
    assert!(live(&g, &key), "live during the grace");
    tokio::time::sleep(GRACE * 4).await;
    assert!(!live(&g, &key), "purged once the grace elapsed");
}

#[tokio::test]
async fn purge_skips_a_qso_reverted_to_an_active_state() {
    let g = grace();
    let id = pancetta_qso::QsoId::new_v4();
    let key = key_of(id);
    let generation = g.keep_live(id, 1500.0);
    // The engine reopened the QSO (its 73 was not sent).
    g.stand_down(&key);
    assert!(!g.purge_if_current(&key, generation));
    tokio::time::sleep(GRACE * 4).await;
    assert!(live(&g, &key), "a reopened QSO keeps its TX key");
}

#[tokio::test]
async fn purge_of_an_earlier_completion_spares_a_recompletion() {
    let g = grace();
    let id = pancetta_qso::QsoId::new_v4();
    let key = key_of(id);
    let first = g.keep_live(id, 1500.0);
    g.stand_down(&key); // reverted
    let third = g.keep_live(id, 1500.0); // completed again
    assert_ne!(first, third);
    assert!(
        !g.purge_if_current(&key, first),
        "the earlier purge stands down"
    );
    assert!(live(&g, &key));
    assert!(g.purge_if_current(&key, third));
    assert!(!live(&g, &key), "the re-completion's own purge removes it");
}

/// PAN-344 AC5: the enabled-target matrix captured at completion.
/// Every enabled outbox destination, including LoTW, is
/// captured, in canonical display order, all Pending; an empty captured
/// set is "uploads disabled"; a preparation failure marks the whole
/// captured set Failed so an enabled target can never disappear from
/// the all-accepted calculation.
#[test]
fn pan344_completion_upload_target_matrix() {
    use super::completion_upload_targets;
    use pancetta_dx::upload_outbox::UploadDestination::*;
    use pancetta_tui::upload_status::{verdict, UploadState, UploadTarget, UploadVerdict};

    let all = completion_upload_targets(&[ClubLog, QrzLogbook, Eqsl, Cqdx, Lotw]);
    assert_eq!(
        all.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        vec![
            UploadTarget::Qrz,
            UploadTarget::ClubLog,
            UploadTarget::Eqsl,
            UploadTarget::Cqdx,
            UploadTarget::Lotw
        ],
        "canonical display order"
    );
    assert!(all
        .iter()
        .all(|(_, s)| s.state == UploadState::Pending && s.reason == "queued"));

    let subset = completion_upload_targets(&[QrzLogbook]);
    assert_eq!(
        subset.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        vec![UploadTarget::Qrz]
    );

    // Empty captured set: disabled, never vacuously all-accepted.
    let none = completion_upload_targets(&[]);
    let registry = super::super::qso_upload_status::UploadStatusRegistry::shared();
    registry.lock().unwrap().register("u-none", "K1ABC", none);
    let snap = registry.lock().unwrap().snapshot();
    assert_eq!(verdict(&snap[0]), UploadVerdict::Disabled);

    // ADIF render failure marks every intended target Failed.
    let uuid = "u-matrix";
    registry.lock().unwrap().register(
        uuid,
        "K1ABC",
        completion_upload_targets(&[ClubLog, QrzLogbook, Lotw]),
    );
    registry.lock().unwrap().record_all(
        uuid,
        UploadState::Failed,
        "upload skipped: ADIF render failed",
    );
    let snap = registry.lock().unwrap().snapshot();
    let entry = snap.iter().find(|s| s.uuid == uuid).unwrap();
    assert_eq!(entry.targets.len(), 3, "no enabled target disappears");
    assert!(entry
        .targets
        .iter()
        .all(|(_, s)| s.state == UploadState::Failed));
    assert_eq!(verdict(entry), UploadVerdict::NeedsAttention);
}
