// PAN-315. TLS trust for the cqdx.io client client: a server certificate
// the rustls verifier does not trust is rejected (test-support/tls.rs).
use super::*;
mod fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-support/tls.rs"
    ));
}
#[tokio::test]
async fn tls_untrusted_cqdx() {
    fixture::rejects_http(CqdxClient::http_builder().no_proxy().build().unwrap()).await;
}
