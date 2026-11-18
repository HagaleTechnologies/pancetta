// PAN-315. TLS trust for the DX cluster client: a server certificate
// the rustls verifier does not trust is rejected (test-support/tls.rs).
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
