// PAN-315. TLS trust for the alert webhook client: a server certificate
// the rustls verifier does not trust is rejected (test-support/tls.rs).
use super::*;
mod fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-support/tls.rs"
    ));
}
#[tokio::test]
async fn tls_untrusted_alerts() {
    fixture::rejects_http(AlertDispatcher::http_builder().no_proxy().build().unwrap()).await;
}
