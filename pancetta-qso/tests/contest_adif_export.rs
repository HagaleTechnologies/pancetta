//! PAN-52: the ADIF export contract for contest-engaged QSOs.
//!
//! `CONTEST_ID` must carry the engaged profile's `adif_contest_id` — a value
//! from the ADIF 3.1.5 Contest_ID enumeration — never pancetta's internal
//! profile slug. A family profile with no single valid id (the built-in
//! `us-state-qso-party`) omits the field entirely: an absent `CONTEST_ID` is
//! legal, unambiguous ADIF, while a slug no logbook service can interpret is
//! not.

use pancetta_qso::adif::AdifProcessor;
use pancetta_qso::contest::catalog::builtin_catalog;
use pancetta_qso::{QsoId, QsoManager, QsoManagerConfig};

fn test_manager() -> QsoManager {
    QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    })
}

/// Renders the ADIF record for `qso_id` exactly the way the upload
/// subscriber and the local `qsos.adi` writer do (coordinator/qso.rs,
/// adif_log_writer.rs): `qso_to_adif` on the stored metadata + contest info,
/// then `generate_record`.
async fn rendered_record(manager: &QsoManager, qso_id: QsoId) -> String {
    let progress = manager.get_qso(qso_id).await.unwrap();
    let processor = AdifProcessor::new();
    let adif_qso =
        processor.qso_to_adif(&progress.metadata, progress.metadata.contest_info.as_ref());
    processor.generate_record(&adif_qso).unwrap()
}

/// The built-in `us-state-qso-party` profile models the *family* of US state
/// QSO parties. ADIF 3.1.5 enumerates state parties per state
/// (`KS-QSO-PARTY`, `SC-QSO-PARTY`, …) with no family-aggregation value, so
/// no single valid `CONTEST_ID` exists for it — the field must be omitted,
/// not stamped with the internal slug.
#[tokio::test]
async fn engaged_family_profile_omits_adif_contest_id() {
    let manager = test_manager();
    let qso_id = manager
        .respond_to_cq_manual("K5TD".to_string(), 1203.0, None)
        .await
        .unwrap();
    let profile = builtin_catalog()
        .into_iter()
        .find(|p| p.id == "us-state-qso-party")
        .unwrap();
    manager
        .engage_contest_profile(qso_id, profile)
        .await
        .unwrap();

    let record = rendered_record(&manager, qso_id).await;
    assert!(
        !record.contains("<CONTEST_ID"),
        "family profile has no single valid ADIF CONTEST_ID, so the field must be \
         omitted; got record: {record}"
    );
}

/// A profile that knows its contest (a future per-state entry, or an
/// operator-defined custom profile) stamps its real ADIF enumeration value
/// on the wire — length specifier included.
#[tokio::test]
async fn profile_with_adif_contest_id_stamps_it_on_the_wire() {
    let manager = test_manager();
    let qso_id = manager
        .respond_to_cq_manual("K5TD".to_string(), 1203.0, None)
        .await
        .unwrap();
    let mut profile = builtin_catalog()
        .into_iter()
        .find(|p| p.id == "us-state-qso-party")
        .unwrap();
    profile.adif_contest_id = Some("KS-QSO-PARTY".to_string());
    manager
        .engage_contest_profile(qso_id, profile)
        .await
        .unwrap();

    let record = rendered_record(&manager, qso_id).await;
    assert!(
        record.contains("<CONTEST_ID:12>KS-QSO-PARTY"),
        "engaged profile's adif_contest_id must reach the wire verbatim; got \
         record: {record}"
    );
}

/// Legacy-compat guard: `QsoMetadata` is persisted as JSON in SQLite, and
/// every pre-PAN-52 row's `ContestInfo` lacks `adif_contest_id`. Those rows
/// must keep deserializing, with the new field defaulting to `None`.
#[test]
fn contest_info_json_without_adif_contest_id_deserializes() {
    let legacy = serde_json::json!({
        "contest_name": "us-state-qso-party",
        "category": "",
        "serials": {"sent": null, "received": null},
        "points": 0,
        "multiplier": null,
    });
    let info: pancetta_qso::states::ContestInfo = serde_json::from_value(legacy)
        .expect("pre-PAN-52 ContestInfo JSON must keep deserializing");
    assert_eq!(info.contest_name, "us-state-qso-party");
    assert!(info.adif_contest_id.is_none());
}
