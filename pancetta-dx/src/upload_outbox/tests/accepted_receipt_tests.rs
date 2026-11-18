use super::*;

#[test]
fn accepted_receipts_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(OUTBOX_FILE_NAME);
    let now = Utc::now();
    let original = Uuid::new_v4();
    let mut store = OutboxStore::open(path.clone()).0;
    store.add(
        vec![NewUpload {
            qso_id: original,
            destination: UploadDestination::Cqdx,
            call: "K1ABC".into(),
            payload: "{}".into(),
        }],
        now,
    );
    OutboxStore::write_atomic(&path, &store.to_bytes().unwrap()).unwrap();
    let legacy = std::fs::read_to_string(&path).unwrap();
    assert!(!legacy.contains("accepted"));
    let (mut store, quarantine) = OutboxStore::open(path.clone());
    assert!(quarantine.is_none());
    assert_eq!(store.counts(UploadDestination::Cqdx).pending, 1);
    let id = store.next_for(UploadDestination::Cqdx).unwrap().id;
    store.accept(id, Some("first".into()), now);
    for n in 0..101 {
        let qso_id = Uuid::from_u128(n + 1);
        store.add(
            vec![NewUpload {
                qso_id,
                destination: UploadDestination::Cqdx,
                call: "K1ABC".into(),
                payload: "{}".into(),
            }],
            now,
        );
        let id = store.next_for(UploadDestination::Cqdx).unwrap().id;
        store.accept(id, Some(format!("id-{n}")), now);
    }
    assert!(store
        .accepted_receipt(original, UploadDestination::Cqdx)
        .is_none());
    let repeated = Uuid::from_u128(1);
    store.add(
        vec![NewUpload {
            qso_id: repeated,
            destination: UploadDestination::Cqdx,
            call: "K1ABC".into(),
            payload: "{}".into(),
        }],
        now,
    );
    let id = store.next_for(UploadDestination::Cqdx).unwrap().id;
    store.accept(id, Some("updated".into()), now);
    OutboxStore::write_atomic(&path, &store.to_bytes().unwrap()).unwrap();
    let (mut reopened, quarantine) = OutboxStore::open_with_capacity(path.clone(), 0);
    assert!(quarantine.is_none());
    assert_eq!(reopened.accepted.len(), ACCEPTED_RECEIPT_LIMIT);
    assert_eq!(
        reopened
            .accepted_receipt(repeated, UploadDestination::Cqdx)
            .unwrap()
            .remote_id,
        "updated"
    );
    assert_eq!(reopened.counts(UploadDestination::Cqdx).pending, 0);
    // Loading a file above the limit still retains only the newest receipts.
    let mut oversized: serde_json::Value =
        serde_json::from_slice(&store.to_bytes().unwrap()).unwrap();
    let receipt = oversized["accepted"][0].clone();
    oversized["accepted"]
        .as_array_mut()
        .unwrap()
        .insert(0, receipt);
    std::fs::write(&path, serde_json::to_vec(&oversized).unwrap()).unwrap();
    reopened = OutboxStore::open(path).0;
    assert_eq!(reopened.accepted.len(), ACCEPTED_RECEIPT_LIMIT);
}
