// PAN-315: every HTTPS/WSS client here rejects an untrusted certificate.
use super::*;
// rationale: the shared fixture also carries the HTTPS helper, which
// this WSS-only module does not call.
#[allow(dead_code)]
mod fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-support/tls.rs"
    ));
}
#[tokio::test]
async fn tls_untrusted_cluster() {
    fixture::rejects_untrusted("wss", |url| async move {
        DxClusterClient::new().connect_websocket(&url).await
    })
    .await;
}
