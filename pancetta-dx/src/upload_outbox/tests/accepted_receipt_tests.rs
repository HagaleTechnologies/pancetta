use super::*;
use jiff::SignedDuration;
use pancetta_core::time::UtcTimestamp;

fn add_and_accept(store: &mut OutboxStore, qso_id: Uuid, remote_id: &str, now: UtcTimestamp) {
    let added = store.add(
        vec![NewUpload {
            qso_id,
            destination: UploadDestination::Cqdx,
            call: "K1ABC".into(),
            payload: "{}".into(),
        }],
        now,
    );
    assert_eq!(added.added, 1, "a receipt must not block a new enqueue");
    let id = store.next_for(UploadDestination::Cqdx).unwrap().id;
    assert!(store.accept(id, Some(remote_id.into()), now).is_some());
}

fn receipts_for(store: &OutboxStore, qso_id: Uuid) -> usize {
    store
        .accepted
        .iter()
        .filter(|r| r.qso_id == qso_id && r.destination == UploadDestination::Cqdx)
        .count()
}

fn remote_id(store: &OutboxStore, qso_id: Uuid) -> Option<&str> {
    store
        .accepted_receipt(qso_id, UploadDestination::Cqdx)
        .map(|r| r.remote_id.as_str())
}

#[test]
fn accepted_receipts_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(OUTBOX_FILE_NAME);
    let now = UtcTimestamp::now();
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

    // 102 accepted IDs into 100 slots evict the two oldest: `original` and UUID 1.
    for n in 1..=101u128 {
        add_and_accept(&mut store, Uuid::from_u128(n), &format!("id-{n}"), now);
    }
    assert_eq!(store.accepted.len(), ACCEPTED_RECEIPT_LIMIT);
    assert!(remote_id(&store, original).is_none());
    assert!(remote_id(&store, Uuid::from_u128(1)).is_none());
    assert_eq!(store.accepted[0].qso_id, Uuid::from_u128(2));
    assert_eq!(remote_id(&store, Uuid::from_u128(2)), Some("id-2"));

    // A repeat of a UUID still in the ledger replaces its receipt in place of
    // adding a second one, moves it to the newest position and evicts nothing.
    let repeated = Uuid::from_u128(50);
    assert_eq!(remote_id(&store, repeated), Some("id-50"));
    add_and_accept(&mut store, repeated, "updated", now);
    assert_eq!(store.accepted.len(), ACCEPTED_RECEIPT_LIMIT);
    assert_eq!(receipts_for(&store, repeated), 1);
    let newest = store.accepted.last().unwrap();
    assert_eq!(
        (newest.qso_id, newest.remote_id.as_str()),
        (repeated, "updated")
    );
    assert_eq!(store.accepted[0].qso_id, Uuid::from_u128(2));
    assert_eq!(remote_id(&store, Uuid::from_u128(2)), Some("id-2"));

    // The receipts and their order survive a write and a reopen with no
    // pending capacity at all.
    OutboxStore::write_atomic(&path, &store.to_bytes().unwrap()).unwrap();
    let (reopened, quarantine) = OutboxStore::open_with_capacity(path.clone(), 0);
    assert!(quarantine.is_none());
    assert_eq!(reopened.accepted, store.accepted);
    assert_eq!(remote_id(&reopened, repeated), Some("updated"));
    assert_eq!(reopened.counts(UploadDestination::Cqdx).pending, 0);

    // A file holding 105 distinct UUID and destination pairs, plus a stale
    // duplicate of `repeated`, reopens to the newest 100 in file order. The
    // timestamps disagree with file order, so they cannot pick the survivors.
    let mut oversized: serde_json::Value =
        serde_json::from_slice(&store.to_bytes().unwrap()).unwrap();
    let accepted = oversized["accepted"].as_array_mut().unwrap();
    let stale = AcceptedReceipt {
        qso_id: repeated,
        destination: UploadDestination::Cqdx,
        remote_id: "stale".into(),
        accepted_at: now + SignedDuration::from_hours(1),
    };
    accepted.insert(10, serde_json::to_value(&stale).unwrap());
    for n in 201..=205u128 {
        let extra = AcceptedReceipt {
            qso_id: Uuid::from_u128(n),
            destination: UploadDestination::Cqdx,
            remote_id: format!("id-{n}"),
            accepted_at: now - SignedDuration::from_hours(1),
        };
        accepted.push(serde_json::to_value(&extra).unwrap());
    }
    let distinct: std::collections::HashSet<_> = accepted
        .iter()
        .map(|r| (r["qso_id"].to_string(), r["destination"].to_string()))
        .collect();
    assert_eq!(distinct.len(), ACCEPTED_RECEIPT_LIMIT + 5);
    std::fs::write(&path, serde_json::to_vec(&oversized).unwrap()).unwrap();
    let (reopened, quarantine) = OutboxStore::open(path);
    assert!(quarantine.is_none());
    assert_eq!(reopened.accepted.len(), ACCEPTED_RECEIPT_LIMIT);
    for n in 2..=6u128 {
        assert!(remote_id(&reopened, Uuid::from_u128(n)).is_none());
    }
    assert_eq!(reopened.accepted[0].qso_id, Uuid::from_u128(7));
    assert_eq!(receipts_for(&reopened, repeated), 1);
    assert_eq!(remote_id(&reopened, repeated), Some("updated"));
    for n in 201..=205u128 {
        let id = format!("id-{n}");
        assert_eq!(remote_id(&reopened, Uuid::from_u128(n)), Some(id.as_str()));
    }
    assert_eq!(
        reopened.accepted.last().unwrap().qso_id,
        Uuid::from_u128(205)
    );
}
