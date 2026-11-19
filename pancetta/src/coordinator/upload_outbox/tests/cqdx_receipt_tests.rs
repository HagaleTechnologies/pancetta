// PAN-378: cqdx receipt persistence through the upload worker.
use super::tests::{cqdx_record_json, schedule, serve_sequence, wait_until, TIMING};
use super::*;
use pancetta_dx::upload_outbox::{UploadDestination::Cqdx, OUTBOX_FILE_NAME};

#[tokio::test]
async fn cqdx_acceptance_persists_remote_id() {
    for duplicate in [false, true] {
        for header in [Some("abc123"), None] {
            let body = format!(r#"{{"duplicate":{duplicate}}}"#);
            let header_line = header.map_or(String::new(), |id| format!("X-QSO-Id: {id}\r\n"));
            let (addr, served) = serve_sequence(vec![format!(
                "HTTP/1.1 201 Created\r\n{header_line}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )]);
            let client = pancetta_cqdx::CqdxClient::new(
                format!("http://{addr}"),
                "pat_0123456789abcdef".into(),
            )
            .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(OUTBOX_FILE_NAME);
            let qso_id = Uuid::new_v4();
            let handle = start_upload_outbox(
                path.clone(),
                Arc::new(LogbookClients {
                    lotw: None,
                    clublog: None,
                    qrz: None,
                    eqsl: None,
                    cqdx: Some(Arc::new(client)),
                }),
                schedule(),
                TIMING,
                Arc::new(std::sync::Mutex::new(UploadHealth::default())),
                None,
                Arc::new(AtomicBool::new(false)),
                None,
            )
            .0;
            handle
                .enqueue(vec![NewUpload {
                    qso_id,
                    destination: Cqdx,
                    call: "K1ABC".into(),
                    payload: cqdx_record_json(),
                }])
                .await;
            wait_until(|| *served.lock().unwrap() == 1).await;
            wait_until(|| {
                let store = OutboxStore::open(path.clone()).0;
                store.is_empty()
                    && (header.is_none() || store.accepted_receipt(qso_id, Cqdx).is_some())
            })
            .await;
            let store = OutboxStore::open(path).0;
            assert_eq!(
                store
                    .accepted_receipt(qso_id, Cqdx)
                    .map(|r| r.remote_id.as_str()),
                header
            );
        }
    }
}
