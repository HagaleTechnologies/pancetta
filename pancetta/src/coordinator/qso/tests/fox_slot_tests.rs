//! PAN-386: coordinator handling of Fox-role QSOs.

use super::*;
use pancetta_core::slot::SlotParity;
use pancetta_core::ResponseStep;
use pancetta_qso::{CallInitiation, QsoManager, QsoManagerConfig};

/// A WSJT-X Hound sends no 73 after the Fox's RR73, so a completed Fox-role
/// QSO is never stashed for the auto-73 resend.
#[tokio::test]
async fn pan386_fox_completion_is_not_stashed_for_auto_73() {
    let manager = QsoManager::new(QsoManagerConfig {
        our_callsign: "KH7Z".into(),
        our_grid: Some("BK29".into()),
        ..QsoManagerConfig::default()
    });
    let id = manager
        .respond_to_caller(
            "W9XYZ".into(),
            360.0,
            Some(SlotParity::Odd),
            ResponseStep::Report,
            Some(-8.0),
            None,
            Some(1240.0),
            false,
            None,
        )
        .await
        .unwrap();
    let mut metadata = manager.get_qso(id).await.unwrap().metadata;
    metadata.initiated_by = CallInitiation::Manual;
    assert!(
        stashes_auto_73(&metadata),
        "control: an ordinary manual completion is stashed"
    );
    assert!(manager.mark_fox_role(id).await);
    let mut fox_metadata = manager.get_qso(id).await.unwrap().metadata;
    fox_metadata.initiated_by = CallInitiation::Manual;
    assert!(fox_metadata.fox);
    assert!(!stashes_auto_73(&fox_metadata));
}
